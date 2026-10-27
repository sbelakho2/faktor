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
import dev.faktor.shared.NativeBillingOverview
import dev.faktor.shared.NativeBillingUsage
import dev.faktor.shared.NativeCreditBalance
import dev.faktor.shared.NativeEntitlementSnapshot
import dev.faktor.shared.NativeIdentity
import dev.faktor.shared.NativeInFlightTxn
import dev.faktor.shared.NativeUsageBuckets
import java.awt.BorderLayout
import java.math.BigInteger
import javax.swing.JPanel

/** The exact limit names the daemon's plan config uses. */
const val USAGE_LIMIT_MAX_TOKENS = "max_tokens_per_period"
const val USAGE_LIMIT_MANAGED_SPEND = "max_managed_spend_micro_per_period"
const val USAGE_LIMIT_MIN_CREDIT = "min_credit_balance_micro"
const val USAGE_LIMIT_ACTIVE_TASKS = "max_active_tasks"
const val USAGE_LIMIT_CHILDREN_PER_TASK = "max_children_per_task"
const val USAGE_LIMIT_ATTEMPTS_PER_TASK = "max_provider_attempts_per_task"

/** One bounded per-task row group; the tail is counted, never rendered. */
const val USAGE_MAX_TASK_ROWS = 20

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
    val billing: NativeBillingOverview? = null,
    val canManageBilling: Boolean = false,
    val manageDisabledReason: String? = null,
    val totals: NativeUsageBuckets?,
    val tasks: List<NativeBillingTaskUsage>,
    /** Total served per-task rows, including the bounded tail. */
    val tasksTotal: Int = 0,
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

    /** The owner-only plan/cancel affordances. */
    fun manageEnabled(): Boolean = canManageBilling && state == "ok"

    /** The usage download is available only when the served capability says so. */
    fun exportEnabled(): Boolean = state == "ok" && billing?.usageExport?.state == "available"

    /** The bounded display lines of the panel: formatted once per model. */
    private val cachedLines: List<String> by lazy { buildLines() }

    fun lines(): List<String> = cachedLines

    private fun buildLines(): List<String> {
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
        if (!canManageBilling && manageDisabledReason != null) {
            out.add("plan change disabled — $manageDisabledReason")
        }
        if (billing != null) {
            when (billing.trial.state) {
                "active" -> out.add(
                    "trial active — ends " +
                        (billing.trial.endsMs?.let { utcSeconds(it) } ?: "unavailable")
                )
                "expired" -> out.add(
                    "[TRIAL EXPIRED] trial ended " +
                        (billing.trial.endsMs?.let { utcSeconds(it) } ?: "unavailable") +
                        " — new tasks are denied"
                )
            }
            for ((label, capability) in listOf(
                "payment" to billing.payment,
                "invoices" to billing.invoices,
                "seats" to billing.seats,
                "tax" to billing.tax,
                "currency" to billing.currency
            )) {
                out.add(
                    if (capability.state == "available") "$label available"
                    else "$label unavailable — ${capability.cause ?: "unavailable"}"
                )
            }
            out.add(
                if (billing.usageExport.state == "available") {
                    "usage export available (${billing.usageExportFormats.joinToString(", ")})"
                } else {
                    "usage export unavailable — ${billing.usageExport.cause ?: "unavailable"}"
                }
            )
            billing.payments?.let { payments ->
                out.add(
                    "invoices ${payments.invoiceCount} recorded · ${payments.openInvoiceCount} open · " +
                        "paid ${minorMoney(payments.paidMinor, payments.currency, payments.currencyExponent)} · " +
                        "refunded ${minorMoney(payments.refundedMinor, payments.currency, payments.currencyExponent)} · " +
                        "outstanding ${minorMoney(payments.outstandingMinor, payments.currency, payments.currencyExponent)}"
                )
                out.add(
                    "seats ${payments.seats ?: "unconfigured"} · " +
                        "currency ${payments.currency ?: "unconfigured"} · " +
                        "tax ${payments.taxJurisdiction ?: payments.taxState} · " +
                        "payment methods ${payments.paymentMethodCount}"
                )
                out.add(
                    "reconciliation ${payments.reconciliationUnreconciled} unreconciled · " +
                        "${payments.reconciliationMatched} matched · " +
                        "${payments.reconciliationConflicts} conflict(s)"
                )
            }
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
            val shownTasks = tasks.take(USAGE_MAX_TASK_ROWS)
            for (task in shownTasks) {
                out.add(
                    "task ${task.taskId} (${task.runId}) — in ${task.totals.inputTokens} · " +
                        "out ${task.totals.outputTokens} · cache ${task.totals.cacheReadTokens}/" +
                        "${task.totals.cacheWriteTokens} · reasoning ${task.totals.reasoningTokens} · " +
                        "managed ${micro(task.totals.managedCostMicro)} · " +
                        "BYOK ${micro(task.totals.byokCostMicro)}"
                )
            }
            val totalTasks = maxOf(tasksTotal, tasks.size)
            if (totalTasks > shownTasks.size) {
                out.add("${totalTasks - shownTasks.size} more task row(s) not shown (bounded to ${shownTasks.size})")
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
                "usage events page — ${plural(itemCount, "row")} · cursor ${cursor ?: "first page"} · " +
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

/** Exact minor-unit money text; no floating point and no second currency code. */
internal fun minorMoney(minor: Long, currency: String?, exponent: Long?): String {
    if (currency == null || exponent == null || exponent < 0 || exponent > 6) return "Unavailable"
    val negative = minor < 0
    val digits = BigInteger.valueOf(minor).abs().toString().padStart(exponent.toInt() + 1, '0')
    val whole = digits.dropLast(exponent.toInt()).trimStart('0').ifEmpty { "0" }
    val fraction = if (exponent == 0L) "" else "." + digits.takeLast(exponent.toInt())
    val grouped = whole.reversed().chunked(3).joinToString(",").reversed()
    val body = "$grouped$fraction"
    val symbol = when (currency) {
        "EUR" -> "€"
        "USD" -> "$"
        "GBP" -> "£"
        else -> null
    }
    return (if (negative) "-" else "") + if (symbol == null) "$body $currency" else "$symbol$body"
}

/** Exact micro-unit display of one money value, memoized by exact value. */
fun micro(value: BigInteger): String = FormattedValues.micro(value)

/** Conventional money summary: exact cents keep two decimals, sub-cent keeps four. */
internal fun summaryUsd(value: BigInteger): String {
    val usd = FormattedValues.usd(value)
    return "$" + if (usd.endsWith("00")) usd.dropLast(2) else usd
}

/** Locale grouping for exact counter values (visual 3.1: 120000 -> 120,000). */
internal fun groupedDecimal(text: String): String {
    val negative = text.startsWith('-')
    val digits = if (negative) text.substring(1) else text
    if (digits.isEmpty() || digits.any { it < '0' || it > '9' }) return text
    val out = StringBuilder()
    for ((index, char) in digits.withIndex()) {
        if (index > 0 && (digits.length - index) % 3 == 0) out.append(',')
        out.append(char)
    }
    return (if (negative) "-" else "") + out
}

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
    hasPrev: Boolean,
    overview: NativeBillingOverview? = null
): UsagePanelModel {
    val canGrant = identity != null && identity.effectiveActions.contains("credits_grant")
    val grantReason = when {
        canGrant -> null
        identity == null ->
            "the control plane did not serve an identity; the granting role is unknown"
        else -> "the ${identity.role} role carries no credits_grant capability (admin only)"
    }
    val canManage = identity != null && identity.effectiveActions.contains("billing_write")
    val manageReason = when {
        canManage -> null
        identity == null ->
            "the control plane did not serve an identity; the owner role is unknown"
        else -> "the ${identity.role} role carries no billing_write capability (owner only)"
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
        billing = overview,
        canManageBilling = canManage,
        manageDisabledReason = manageReason,
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
        tasks = usage.fold.perTask.take(USAGE_MAX_TASK_ROWS),
        tasksTotal = usage.fold.perTask.size,
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

        /** Owner-only plan change (default: no-op for legacy callers). */
        fun onChangePlan() {}

        /** Owner-only cancellation (default: no-op for legacy callers). */
        fun onCancelSubscription() {}

        /** Bounded usage download (default: no-op for legacy callers). */
        fun onExportUsage() {}
    }

    private val statusLabel = WrappedLabel("Account not connected")

    private val periodLabel = WrappedLabel("No period is served yet.")

    private val tokenLabel = WrappedLabel("Tokens — not reported")

    private val tokenBar = javax.swing.JProgressBar(0, 100).apply {
        a11y("Token quota", "Share of the plan token limit used this period.")
        isStringPainted = false
        isVisible = false
        alignmentX = java.awt.Component.LEFT_ALIGNMENT
    }

    private val spendLabel = WrappedLabel("Managed spend — not reported")

    private val creditsLabel = WrappedLabel("Credits — not reported")

    private val planLabel = WrappedLabel("Plan — not reported")

    private val linesArea = compactArea(10).apply {
        a11y("Usage detail", "Exact token, spend and credit lines.")
    }

    private val advancedToggle = secondaryButton("Show advanced")

    private val advancedBody = JPanel(BorderLayout())

    private val previousButton = secondaryButton("Previous page")

    private val nextButton = secondaryButton("Next page")

    private val grantButton = primaryButton("Grant credits…")

    private val exportButton = secondaryButton("Download usage…")

    private val changePlanButton = secondaryButton("Change plan…")

    private val cancelButton = secondaryButton("Cancel subscription…")

    private var listener: Listener? = null

    private var model: UsagePanelModel? = null
    private var applied = false

    init {
        linesArea.isOpaque = false
        val periodBody = pageColumn(gap = Spacing.S, padding = 0)
        periodBody.add(periodLabel)
        periodBody.add(tokenLabel)
        periodBody.add(tokenBar)
        periodBody.add(spendLabel)
        periodBody.add(creditsLabel)
        periodBody.add(planLabel)
        advancedToggle.addActionListener {
            advancedBody.isVisible = advancedToggle.isSelected
            advancedToggle.text = if (advancedToggle.isSelected) {
                "Hide advanced"
            } else {
                "Show advanced"
            }
            revalidate()
            repaint()
        }
        advancedBody.isOpaque = false
        advancedBody.add(insetScroll(linesArea), BorderLayout.CENTER)
        advancedBody.isVisible = false
        val body = pageColumn()
        body.add(card("Current period", periodBody))
        body.add(vSpace(Spacing.M))
        body.add(actionRow(previousButton, nextButton, grantButton))
        body.add(vSpace(Spacing.S))
        body.add(actionRow(exportButton, changePlanButton, cancelButton))
        body.add(vSpace(Spacing.M))
        body.add(advancedToggle)
        body.add(vSpace(Spacing.XS))
        body.add(advancedBody)
        add(panelHeader(statusLabel), BorderLayout.NORTH)
        add(pageScroll(body), BorderLayout.CENTER)

        previousButton.addActionListener { listener?.onPreviousPage() }
        nextButton.addActionListener { listener?.onNextPage() }
        grantButton.addActionListener { listener?.onGrantCredits() }
        exportButton.addActionListener { listener?.onExportUsage() }
        changePlanButton.addActionListener { listener?.onChangePlan() }
        cancelButton.addActionListener { listener?.onCancelSubscription() }
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

    /**
     * The lazy Account empty state: no control-plane credential is configured
     * (or the daemon is not running), so no identity/billing read was made and
     * no number is rendered.
     */
    fun setNotConnected() {
        applied = true
        model = null
        linesArea.text = "Account not connected. Sign in from Settings to use billing and usage."
        statusLabel.text = "Account not connected"
        statusLabel.foreground = mutedForeground()
        periodLabel.fullText = "No account is connected; no period numbers are served."
        tokenLabel.fullText = "Tokens — not reported"
        tokenBar.isVisible = false
        spendLabel.fullText = "Managed spend — not reported"
        creditsLabel.fullText = "Credits — not reported"
        planLabel.fullText = "Plan — not reported"
        previousButton.isEnabled = false
        nextButton.isEnabled = false
        grantButton.isEnabled = false
        exportButton.isEnabled = false
        changePlanButton.isEnabled = false
        cancelButton.isEnabled = false
    }

    fun statusText(): String = statusLabel.fullText

    fun model(): UsagePanelModel? = model

    fun nextEnabled(): Boolean = model?.nextEnabled() ?: false

    fun prevEnabled(): Boolean = model?.prevEnabled() ?: false

    fun grantEnabled(): Boolean = model?.grantEnabled() ?: false

    fun lines(): List<String> = model?.lines() ?: emptyList()

    /** The bounded token progress percentage (0..100), or null when unknown. */
    fun tokenProgressPercent(): Int? {
        val quota = model?.quotas?.firstOrNull { it.limit == USAGE_LIMIT_MAX_TOKENS } ?: return null
        val observed = quota.observed ?: return null
        val limit = quota.value
        if (limit.signum() <= 0) return null
        return FormattedValues.percent(observed, limit)
    }

    /** The primary currency/token lines (smoke observable). */
    fun periodText(): String = buildString {
        append(periodLabel.fullText).append('\n')
        append(tokenLabel.fullText).append('\n')
        append(spendLabel.fullText).append('\n')
        append(creditsLabel.fullText).append('\n')
        append(planLabel.fullText).append('\n')
    }

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

    internal fun clickExport() {
        exportButton.doClick()
    }

    internal fun clickChangePlan() {
        changePlanButton.doClick()
    }

    internal fun clickCancel() {
        cancelButton.doClick()
    }

    private fun apply(value: UsagePanelModel?) {
        if (applied && model == value) return
        applied = true
        model = value
        val text = value?.lines() ?: emptyList()
        linesArea.text = text.joinToString("\n")
        linesArea.caretPosition = 0
        statusLabel.text = when (value?.state) {
            "ok" -> "usage · current period"
            "disabled" -> "usage: billing disabled locally"
            "unavailable" -> "usage: unavailable"
            else -> "usage: -"
        }
        applyPeriod(value)
        previousButton.isEnabled = value?.prevEnabled() == true
        nextButton.isEnabled = value?.nextEnabled() == true
        grantButton.isEnabled = value?.grantEnabled() == true
        exportButton.isEnabled = value?.exportEnabled() == true
        changePlanButton.isEnabled = value?.manageEnabled() == true
        cancelButton.isEnabled = value?.manageEnabled() == true
        // Quota breach or a non-active subscription renders as an alert
        // state (the line markers `[EXCEEDED]`/`[EXPIRED]`/`[GRACE]` stay the
        // authoritative text cue).
        val alert = value != null && (
            value.quotas.any { it.exceeded == true } ||
                value.subscription == "expired" ||
                value.subscription == "grace" ||
                value.subscription == "canceled"
            )
        statusLabel.foreground = if (alert) {
            semanticForeground(SemanticState.NEGATIVE)
        } else {
            textForeground()
        }
    }

    /** The humanized period card: tokens bar, currency spend, credits, plan. */
    private fun applyPeriod(value: UsagePanelModel?) {
        if (value == null || value.state != "ok") {
            periodLabel.fullText = when (value?.state) {
                "disabled" -> "Billing is disabled locally; no period numbers are served."
                "unavailable" -> "The period is unavailable; no numbers are served."
                else -> "No period is served yet."
            }
            tokenLabel.fullText = "Tokens — not reported"
            tokenBar.isVisible = false
            spendLabel.fullText = "Managed spend — not reported"
            creditsLabel.fullText = "Credits — not reported"
            planLabel.fullText = "Plan — not reported"
            return
        }
        val tokens = value.totals?.totalTokens()
        val tokenQuota = value.quotas.firstOrNull { it.limit == USAGE_LIMIT_MAX_TOKENS }
        tokenLabel.fullText = if (tokens == null) {
            "Tokens — not reported"
        } else if (tokenQuota?.observed != null && tokenQuota.value.signum() > 0) {
            "Tokens — " + groupedDecimal(tokens.toString()) + " of " +
                groupedDecimal(tokenQuota.value.toString()) + " used this period"
        } else {
            "Tokens — " + groupedDecimal(tokens.toString()) + " used this period"
        }
        val percent = tokenProgressPercent()
        if (percent != null) {
            tokenBar.value = percent
            tokenBar.isVisible = true
            tokenBar.toolTipText = "Tokens used this period: $percent% of the plan limit"
        } else {
            tokenBar.isVisible = false
        }
        val managedQuota = value.quotas.firstOrNull { it.limit == USAGE_LIMIT_MANAGED_SPEND }
        val managed = value.totals?.managedCostMicro
        spendLabel.fullText = if (managed == null) {
            "Managed spend — not reported"
        } else if (managedQuota != null && managedQuota.value.signum() > 0) {
            "Managed spend — " + summaryUsd(managed) + " of " + summaryUsd(managedQuota.value) +
                " this period"
        } else {
            "Managed spend — " + summaryUsd(managed) + " this period"
        }
        val credits = value.credits
        creditsLabel.fullText = if (credits == null) {
            "Credits — not reported"
        } else {
            "Credits — " + summaryUsd(credits.balanceMicro()) + " available"
        }
        planLabel.fullText = buildString {
            append("Plan — ").append(value.planId ?: "none")
            if (value.planId != null && !value.planFound) {
                append(" (not defined in the billing configuration)")
            }
            append(" · ").append(value.organization ?: "no organization")
            append('\n')
            append("Subscription — ").append(
                value.subscription.ifEmpty { "unavailable" }
            )
        }
        val breach = value.quotas.firstOrNull {
            it.exceeded == true && it.limit in setOf(
                USAGE_LIMIT_MAX_TOKENS,
                USAGE_LIMIT_MANAGED_SPEND,
                USAGE_LIMIT_MIN_CREDIT
            )
        }
        periodLabel.fullText = when (breach?.limit) {
            USAGE_LIMIT_MAX_TOKENS ->
                "Token cap exceeded — new tasks are denied until the cap or plan is raised."
            USAGE_LIMIT_MANAGED_SPEND ->
                "Managed spend cap exceeded — new managed tasks are denied until the cap is raised."
            USAGE_LIMIT_MIN_CREDIT ->
                "Credit balance is below the plan floor — credit-funded tasks are denied."
            else -> "Usage for the current billing period."
        }
        periodLabel.foreground = if (breach == null) {
            mutedForeground()
        } else {
            semanticForeground(SemanticState.NEGATIVE)
        }
    }
}
