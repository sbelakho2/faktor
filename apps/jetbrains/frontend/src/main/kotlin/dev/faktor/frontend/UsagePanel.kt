// The commercial-metering panel: current-period tokens and cost split
// (managed vs BYOK), quota/limit progress naming the EXACT limit from the
// entitlement snapshot, the subscription state (active/expired/canceled/
// grace/unavailable), the credit balance from the control plane, cursor
// pagination and the role-aware grant-credits affordance.
//
// Pure projection: UsagePanelModel carries every rendered fact (so the smoke
// drives it without a display), the Swing panel renders it, and a refusal is
// an explicit disabled/unavailable model with its reason — zeros are NEVER
// fabricated behind it. Actions are delivered to a Listener.
package dev.faktor.frontend

import dev.faktor.shared.MicroMoney
import dev.faktor.shared.NativeBillingTaskUsage
import dev.faktor.shared.NativeBillingUsage
import dev.faktor.shared.NativeCreditBalance
import dev.faktor.shared.NativeEntitlementSnapshot
import dev.faktor.shared.NativeIdentity
import dev.faktor.shared.NativeInFlightTxn
import dev.faktor.shared.NativeUsageBuckets
import java.awt.BorderLayout
import java.math.BigInteger
import java.awt.FlowLayout
import javax.swing.JButton
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane

/** The exact limit names the daemon's plan config uses. */
const val USAGE_LIMIT_MAX_TOKENS = "max_tokens_per_period"
const val USAGE_LIMIT_MANAGED_SPEND = "max_managed_spend_micro_per_period"
const val USAGE_LIMIT_MIN_CREDIT = "min_credit_balance_micro"
const val USAGE_LIMIT_ACTIVE_TASKS = "max_active_tasks"
const val USAGE_LIMIT_CHILDREN_PER_TASK = "max_children_per_task"
const val USAGE_LIMIT_ATTEMPTS_PER_TASK = "max_provider_attempts_per_task"

/** One quota/limit row; a `ceiling` breaches at/above, a `floor` below. */
data class UsageQuotaRow(
    val limit: String,
    /** Exact limit value (money for a `*_micro` name, else a counter). */
    val value: BigInteger,
    val observed: BigInteger?,
    val exceeded: Boolean?,
    val kind: String,
    val reason: String?
) {
    /** One bounded display line; `[EXCEEDED]` leads so it survives a clamp. */
    fun line(): String {
        val head = if (exceeded == true) "[EXCEEDED] " else ""
        val observedText = observed ?: return head +
            "quota $limit — limit $value (observed not served: ${reason ?: "unavailable"})"
        val unit = if (MicroMoney.isMicroLimitName(limit)) {
            micro(observedText) + " / limit $value"
        } else {
            "$observedText / limit $value"
        }
        return "$head" + "quota $limit — observed $unit"
    }
}

/** The pure projection the panel renders (and the smoke asserts on). */
data class UsagePanelModel(
    /** ok | disabled | unavailable. */
    val state: String,
    val reason: String?,
    val organization: String?,
    val planId: String?,
    val planFound: Boolean,
    /** active | expired | canceled | grace | unavailable. */
    val subscription: String,
    val subscriptionStatus: String?,
    val subscriptionExpiresMs: Long?,
    val subscriptionActive: Boolean?,
    val totals: NativeUsageBuckets?,
    val tasks: List<NativeBillingTaskUsage>,
    val credits: NativeCreditBalance?,
    val quotas: List<UsageQuotaRow>,
    val inFlight: List<NativeInFlightTxn>,
    val cursor: String?,
    val nextCursor: String?,
    val itemCount: Int,
    val hasPrev: Boolean,
    val canGrantCredits: Boolean,
    val grantDisabledReason: String?
) {
    fun nextEnabled(): Boolean = nextCursor != null

    fun prevEnabled(): Boolean = hasPrev

    /** The grant affordance: visible for admin principals, disabled otherwise
     * (and disabled whenever the billing surface itself is not usable). */
    fun grantEnabled(): Boolean = canGrantCredits && state == "ok"

    /** The bounded display lines of the panel. */
    fun lines(): List<String> {
        val out = ArrayList<String>()
        when (state) {
            "disabled" -> out.add(
                "billing disabled locally" + (if (reason == null) "" else " — $reason")
            )
            "unavailable" -> out.add(
                "usage unavailable" + (if (reason == null) "" else " — $reason")
            )
        }
        if (organization != null) {
            val planGap = if (planId != null && !planFound) {
                " (plan not defined in the billing config — admission denies it)"
            } else {
                ""
            }
            out.add("organization $organization · plan ${planId ?: "none"}$planGap")
        }
        val expires = if (subscriptionExpiresMs == null) {
            ""
        } else {
            " (expires ${utcSeconds(subscriptionExpiresMs)})"
        }
        when (subscription) {
            "active" -> out.add("subscription active$expires")
            "expired" -> out.add("[EXPIRED] subscription expired$expires — new tasks are denied")
            "canceled" -> out.add("[CANCELED] subscription canceled$expires")
            "grace" -> out.add(
                "[GRACE] subscription lapsed: the durable row says active but the effective " +
                    "snapshot is inactive$expires — new tasks are denied"
            )
            else -> out.add("subscription unavailable (the snapshot serves no subscription)")
        }
        if (totals != null) {
            out.add(
                "period tokens — in ${totals.inputTokens} · out ${totals.outputTokens} · " +
                    "cache read ${totals.cacheReadTokens} · cache write ${totals.cacheWriteTokens} · " +
                    "reasoning ${totals.reasoningTokens} · total ${totals.totalTokens()}"
            )
            out.add(
                "period spend — managed ${micro(totals.managedCostMicro)} · " +
                    "BYOK ${micro(totals.byokCostMicro)} · provider cost " +
                    "${micro(totals.providerCostMicro)} · events ${totals.events} " +
                    "(corrected ${totals.correctedEvents})"
            )
            for (task in tasks) {
                out.add(
                    "task ${task.taskId} (${task.runId}) — in ${task.totals.inputTokens} · " +
                        "out ${task.totals.outputTokens} · cache ${task.totals.cacheReadTokens}/" +
                        "${task.totals.cacheWriteTokens} · reasoning ${task.totals.reasoningTokens} · " +
                        "managed ${micro(task.totals.managedCostMicro)} · " +
                        "BYOK ${micro(task.totals.byokCostMicro)}"
                )
            }
        }
        for (quota in quotas) {
            out.add(quota.line())
        }
        if (credits != null) {
            out.add(
                "credits balance ${micro(credits.balanceMicro())} — granted " +
                    "${micro(credits.grantedMicro)} · consumed ${micro(credits.consumedMicro)} · " +
                    "refunded ${micro(credits.refundedMicro)} · held ${micro(credits.heldMicro)} · " +
                    "pending consumes ${credits.pendingConsumes}"
            )
        }
        for (txn in inFlight) {
            // `endedMs` is a `val` of NativeInFlightTxn in the shared module;
            // Kotlin forbids smart casts across module boundaries, so bind the
            // nullable locally before formatting (compiles under Gradle's
            // separate shared/frontend modules as well as the single-module
            // kotlinc smoke).
            val ended = txn.endedMs?.let { " — ended ${utcSeconds(it)}" } ?: ""
            out.add(
                "in-flight ${txn.kind} ${txn.id} (${txn.reference}) since " +
                    utcSeconds(txn.startedMs) + ended
            )
        }
        if (state == "ok" || cursor != null || nextCursor != null) {
            out.add(
                "usage events page — $itemCount row(s) · cursor ${cursor ?: "first page"} · " +
                    "next ${nextCursor ?: "none"}"
            )
        }
        if (canGrantCredits) {
            out.add("role grants credits (credits_grant capability present)")
        } else {
            out.add("grant credits disabled — ${grantDisabledReason ?: "not permitted"}")
        }
        return out.map { bound(it, 480) }
    }
}

/** Exact micro-unit display of one money value (never scientific notation). */
fun micro(value: BigInteger): String = MicroMoney.microText(value)

private fun utcSeconds(ms: Long): String {
    val instant = java.time.Instant.ofEpochMilli(ms)
    return instant.toString().substringBefore('.') + "Z"
}

/**
 * Build the panel model from the served payloads plus the last refusal.
 * `ok` requires BOTH the entitlement snapshot and the usage fold; a
 * `billing_disabled` refusal renders "billing disabled locally" with no
 * numbers. The grant affordance follows the identity's `credits_grant`
 * effective action (the server's admin rule).
 */
fun usagePanelModelOf(
    identity: NativeIdentity?,
    entitlements: NativeEntitlementSnapshot?,
    usage: NativeBillingUsage?,
    refusalCode: String?,
    refusalReason: String?,
    cursor: String?,
    hasPrev: Boolean
): UsagePanelModel {
    val canGrant = identity != null && identity.effectiveActions.contains("credits_grant")
    val grantReason = when {
        canGrant -> null
        identity == null ->
            "the control plane did not serve an identity; the granting role is unknown"
        else -> "the ${identity.role} role carries no credits_grant capability (admin only)"
    }
    val (subscription, status, expires, active) = subscriptionOf(entitlements)
    val common = UsagePanelModel(
        state = "ok",
        reason = null,
        organization = identity?.organization ?: entitlements?.organizationId,
        planId = entitlements?.planId,
        planFound = entitlements?.planFound ?: false,
        subscription = subscription,
        subscriptionStatus = status,
        subscriptionExpiresMs = expires,
        subscriptionActive = active,
        totals = null,
        tasks = emptyList(),
        credits = null,
        quotas = emptyList(),
        inFlight = entitlements?.inFlight ?: emptyList(),
        cursor = cursor,
        nextCursor = usage?.nextCursor,
        itemCount = usage?.itemCount ?: 0,
        hasPrev = hasPrev,
        canGrantCredits = canGrant,
        grantDisabledReason = grantReason
    )
    if (refusalCode != null) {
        return common.copy(
            state = if (refusalCode == "billing_disabled") "disabled" else "unavailable",
            reason = refusalReason,
            grantDisabledReason = if (canGrant) {
                "the billing surface is disabled or unavailable; a grant cannot be attempted"
            } else {
                grantReason
            }
        )
    }
    if (entitlements == null || usage == null) {
        return common.copy(
            state = "unavailable",
            reason = "the control plane served no usage/entitlement payload",
            grantDisabledReason = if (canGrant) {
                "the control plane served no usage/entitlement payload; a grant cannot be attempted"
            } else {
                grantReason
            }
        )
    }
    return common.copy(
        totals = usage.fold.totals,
        tasks = usage.fold.perTask,
        credits = usage.credits,
        quotas = quotaRowsOf(entitlements)
    )
}

private data class SubscriptionView(
    val state: String,
    val status: String?,
    val expiresMs: Long?,
    val active: Boolean?
)

private fun subscriptionOf(snapshot: NativeEntitlementSnapshot?): SubscriptionView {
    if (snapshot == null) {
        return SubscriptionView("unavailable", null, null, null)
    }
    val status = snapshot.subscriptionStatus
    if (snapshot.subscriptionActive) {
        return SubscriptionView("active", status, snapshot.subscriptionExpiresMs, true)
    }
    if (status == "expired" || status == "canceled") {
        return SubscriptionView(status, status, snapshot.subscriptionExpiresMs, false)
    }
    if (status == "active") {
        // The durable row says active but the effective snapshot is inactive:
        // the expiry has passed. Honest lapse rendering — never "active".
        return SubscriptionView("grace", status, snapshot.subscriptionExpiresMs, false)
    }
    return SubscriptionView("unavailable", status, snapshot.subscriptionExpiresMs, false)
}

private fun quotaRowsOf(snapshot: NativeEntitlementSnapshot): List<UsageQuotaRow> {
    val rows = ArrayList<UsageQuotaRow>()
    for ((limit, value) in snapshot.limits.entries.sortedBy { it.key }) {
        var observed: BigInteger? = null
        var reason: String? = null
        when (limit) {
            USAGE_LIMIT_MAX_TOKENS -> observed = BigInteger.valueOf(snapshot.totalTokens)
            USAGE_LIMIT_MANAGED_SPEND -> observed = snapshot.managedSpendMicro
            USAGE_LIMIT_MIN_CREDIT -> observed = snapshot.credits.balanceMicro()
            else -> reason = "no observed counter is served for this limit"
        }
        // Exact BigInteger comparison: a u64 money limit never rounds.
        val floor = limit == USAGE_LIMIT_MIN_CREDIT
        val exceeded = if (observed == null) null else if (floor) observed < value else observed >= value
        rows.add(
            UsageQuotaRow(
                limit = limit,
                value = value,
                observed = observed,
                exceeded = exceeded,
                kind = if (floor) "floor" else "ceiling",
                reason = reason
            )
        )
    }
    return rows
}

class UsagePanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onNextPage()

        fun onPreviousPage()

        fun onGrantCredits()
    }

    private val statusLabel = JLabel("usage: -")

    private val linesArea = compactArea(10)

    private val previousButton = JButton("Previous page")

    private val nextButton = JButton("Next page")

    private val grantButton = JButton("Grant credits…")

    private var listener: Listener? = null

    private var model: UsagePanelModel? = null

    init {
        val body = JPanel(BorderLayout(0, 4))
        body.border = javax.swing.BorderFactory.createEmptyBorder(4, 6, 4, 6)
        body.add(statusLabel, BorderLayout.NORTH)
        body.add(JScrollPane(linesArea), BorderLayout.CENTER)
        val actions = JPanel(FlowLayout(FlowLayout.LEFT, 4, 0))
        actions.add(previousButton)
        actions.add(nextButton)
        actions.add(grantButton)
        body.add(actions, BorderLayout.SOUTH)
        add(body, BorderLayout.NORTH)

        previousButton.addActionListener { listener?.onPreviousPage() }
        nextButton.addActionListener { listener?.onNextPage() }
        grantButton.addActionListener { listener?.onGrantCredits() }
        apply(null)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the panel contents with one model (null = nothing read yet). */
    fun setModel(value: UsagePanelModel?) {
        apply(value)
    }

    /** The explicit refusal/disabled state with its reason. */
    fun setUnavailable(reason: String, disabled: Boolean = false) {
        apply(
            usagePanelModelOf(
                identity = null,
                entitlements = null,
                usage = null,
                refusalCode = if (disabled) "billing_disabled" else "unavailable",
                refusalReason = reason,
                cursor = null,
                hasPrev = false
            )
        )
    }

    fun model(): UsagePanelModel? = model

    fun nextEnabled(): Boolean = model?.nextEnabled() ?: false

    fun prevEnabled(): Boolean = model?.prevEnabled() ?: false

    fun grantEnabled(): Boolean = model?.grantEnabled() ?: false

    fun lines(): List<String> = model?.lines() ?: emptyList()

    /** Test hook: clicks the paging/grant controls exactly like a user. */
    internal fun clickNext() {
        nextButton.doClick()
    }

    internal fun clickPrevious() {
        previousButton.doClick()
    }

    internal fun clickGrant() {
        grantButton.doClick()
    }

    private fun apply(value: UsagePanelModel?) {
        model = value
        val text = value?.lines() ?: emptyList()
        linesArea.text = text.joinToString("\n")
        linesArea.caretPosition = 0
        statusLabel.text = when (value?.state) {
            "ok" -> "usage: ${value.organization ?: "-"} · ${value.subscription}"
            "disabled" -> "usage: billing disabled locally"
            "unavailable" -> "usage: unavailable"
            else -> "usage: -"
        }
        previousButton.isEnabled = value?.prevEnabled() == true
        nextButton.isEnabled = value?.nextEnabled() == true
        grantButton.isEnabled = value?.grantEnabled() == true
        // Quota breach or a non-active subscription renders as an alert
        // state (the line markers `[EXCEEDED]`/`[EXPIRED]`/`[GRACE]` stay the
        // authoritative text cue).
        val alert = value != null && (
            value.quotas.any { it.exceeded == true } ||
                value.subscription == "expired" ||
                value.subscription == "grace" ||
                value.subscription == "canceled"
            )
        statusLabel.foreground = if (alert) java.awt.Color(0xC0, 0x39, 0x2B) else null
    }
}
