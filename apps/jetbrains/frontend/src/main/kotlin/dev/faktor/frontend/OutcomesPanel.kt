// I07 — the Engineering Outcome Ledger surface (GET /native/outcomes).
//
// Pure projection: [OutcomesPanelModel] carries every rendered fact (exact
// metric names/definitions from docs/outcome-ledger.md, served counts and
// denominators, scan honesty) so the smoke drives it without a display; the
// Swing panel renders it. Money stays a BigInteger end-to-end (MicroMoney
// helpers, never scientific notation); an empty verified denominator renders
// the exact `No verified changes yet`, both cost metrics are gated on the
// MONEY-SETTLED verified cohort (`verifiedChangesMoneyComplete` — a
// verified-but-money-pending run never renders a fabricated $0, and the
// excluded count is surfaced), an empty rate denominator renders `—` (never
// a measured 0 rate), an unmeasured counterfactual renders `not measured`
// (never `0` as a savings/free claim), and a read failure is the T29
// `Cost unavailable` state with no monetary value at all.
package dev.faktor.frontend

import dev.faktor.shared.MicroMoney
import dev.faktor.shared.ProtocolOutcomeCursor
import dev.faktor.shared.ProtocolOutcomeMetrics
import dev.faktor.shared.ProtocolOutcomesPage
import dev.faktor.shared.ProtocolRunOutcome
import dev.faktor.shared.ProtocolValueEntitlementsReport
import java.awt.BorderLayout
import java.math.BigInteger
import javax.swing.BoxLayout
import javax.swing.JPanel

/** Exact empty-denominator copy (never a zero rate claim). */
const val OUTCOMES_NO_VERIFIED_CHANGES = "No verified changes yet"

/**
 * The gate copy for a verified cohort whose money is NOT settled: the served
 * Successful-Run-Cost/VCSC values held their folded zero, which must never be
 * rendered as a measured $0.
 */
const val OUTCOMES_NO_MONEY_SETTLED_CHANGES = "No money-settled verified changes yet"

/** Exact empty-denominator glyph for rates (never a measured 0 rate). */
const val OUTCOMES_UNAVAILABLE_VALUE = "\u2014"

/** Exact unmeasured copy (an unrecorded counterfactual is never a 0 claim). */
const val OUTCOMES_NOT_MEASURED = "not measured"

/** T29 copy (state matrix, verbatim). */
const val OUTCOMES_COST_UNAVAILABLE = "Cost unavailable"
const val OUTCOMES_COST_UNAVAILABLE_COPY =
    "The current authority has not supplied a cost value."

/** Max run rows one page renders (the panel requests 25; the bound is ours). */
const val OUTCOMES_MAX_RUN_ROWS = 200

/** The page size the panel requests from `GET /native/outcomes`. */
const val OUTCOMES_PAGE_LIMIT = 25L

/** The exact `outcome_ledger` hook the outcomes read enforces. */
const val OUTCOMES_ENTITLEMENT_HOOK = "outcome_ledger"

/**
 * The value-entitlements status line of one successful report: the resolved
 * plan, the `outcome_ledger` decision (granted, or refused with its exact
 * cause) and the granted/total count. Deterministic and exact — the panel
 * renders this string verbatim, and a missing decision renders as refused,
 * never as a granted claim.
 */
fun valueEntitlementsStatusLine(report: ProtocolValueEntitlementsReport): String {
    val plan = if (report.planFound && !report.planId.isNullOrEmpty()) {
        report.planId
    } else {
        "unresolved"
    }
    val decision = report.decisions.firstOrNull { it.hook == OUTCOMES_ENTITLEMENT_HOOK }
    val ledger = when {
        decision == null -> "outcome_ledger refused (no decision served)"
        decision.granted -> "outcome_ledger granted"
        else -> "outcome_ledger refused (${decision.cause ?: "unknown"})"
    }
    val granted = report.decisions.count { it.granted }
    return "value entitlements: plan $plan · $ledger · $granted/${report.decisions.size} granted"
}

/**
 * The value-entitlements status line of one failed report read: a disabled
 * billing service renders "billing disabled locally" (the shared typed 409),
 * anything else names the refusal. The reason is bounded — the panel line is
 * a status, not a diagnostic dump.
 */
fun valueEntitlementsRefusalStatusLine(code: String?, reason: String): String {
    if (code == "billing_disabled") {
        return "value entitlements: billing disabled locally"
    }
    val bounded = if (reason.length > 120) reason.take(120) + "…" else reason
    return "value entitlements unavailable: $bounded"
}

/** One metric row: exact name, exact rendered value, served detail, definition. */
data class OutcomeMetricRow(
    val name: String,
    val value: String,
    val detail: String,
    val explanation: String
)

/** One ledger run row (newest-first page). */
data class OutcomeRunRow(
    val runId: String,
    val taskClass: String,
    val riskClass: String,
    val verified: Boolean,
    val firstVerificationPass: Boolean,
    /** Null when the money is not settled (`moneyState != complete`). */
    val modelSpendMicro: String?,
    /** Null when the money is not settled (`moneyState != complete`). */
    val reworkSpendMicro: String?,
    val wallTimeMs: Long,
    val completedAtMs: Long
)

/** The pure projection the Outcomes panel renders (and the smoke asserts). */
data class OutcomesPanelModel(
    /** not_read | ok | unavailable. */
    val state: String,
    val reason: String?,
    val metrics: ProtocolOutcomeMetrics?,
    val metricRows: List<OutcomeMetricRow>,
    val runRows: List<OutcomeRunRow>,
    val nextCursor: ProtocolOutcomeCursor?,
    val scanned: Long,
    val truncated: Boolean,
    val pagingExhausted: Boolean,
    /**
     * The typed refusal code of a failed read (`value_entitlement_refused`,
     * `billing_disabled`, `store_error`, …); null while nothing refused. The
     * panel renders it verbatim so a typed 403/409 is never flattened into
     * an anonymous "unavailable".
     */
    val refusalCode: String? = null,
    /**
     * The value-entitlements status line (`GET /native/value-entitlements`)
     * of the configured control-plane organization: the plan resolution, the
     * `outcome_ledger` decision and the granted/total count, or the typed
     * refusal ("billing disabled locally" for the 409). Null while no
     * organization is configured — a local read is never dressed up as an
     * entitlement.
     */
    val entitlementsStatus: String? = null
) {
    fun olderEnabled(): Boolean = nextCursor != null

    fun hasMonetaryValue(): Boolean = metricRows.any { it.value.startsWith("$") }

    fun metricNames(): List<String> = metricRows.map { it.name }

    fun metricValue(name: String): String? = metricRows.firstOrNull { it.name == name }?.value

    /** The bounded display lines (smoke-visible; exact metric names). */
    fun lines(): List<String> = metricRows.map { "${it.name}: ${it.value} — ${it.detail}" } +
        runRows.map { "run ${it.runId} [$it.taskClass/$it.riskClass] verified=${it.verified}" } +
        listOfNotNull(entitlementsStatus)
}

/**
 * Build the panel model from one served page plus the last refusal. A null
 * page without a refusal is the honest `not_read` state (nothing invented);
 * a refusal is the T29 `unavailable` state with NO monetary rows; the typed
 * refusal code and the value-entitlements status line ride the model so both
 * are rendered verbatim.
 */
fun outcomesPanelModelOf(
    page: ProtocolOutcomesPage?,
    refusalReason: String? = null,
    refusalCode: String? = null,
    entitlementsStatus: String? = null
): OutcomesPanelModel {
    if (page == null) {
        return OutcomesPanelModel(
            state = if (refusalReason == null) "not_read" else "unavailable",
            reason = refusalReason,
            metrics = null,
            metricRows = emptyList(),
            runRows = emptyList(),
            nextCursor = null,
            scanned = 0L,
            truncated = false,
            pagingExhausted = false,
            refusalCode = refusalCode,
            entitlementsStatus = entitlementsStatus
        )
    }
    val metrics = page.metrics
    val money = moneyGlyph(metrics)
    if (money == null) {
        return OutcomesPanelModel(
            state = "unavailable",
            reason = "the served outcome metrics carry a malformed money value",
            metrics = null,
            metricRows = emptyList(),
            runRows = emptyList(),
            nextCursor = null,
            scanned = metrics.scanned,
            truncated = metrics.truncated,
            pagingExhausted = page.nextCursor == null,
            refusalCode = refusalCode,
            entitlementsStatus = entitlementsStatus
        )
    }
    val rows = ArrayList<OutcomeMetricRow>()
    val verifiedDenominator = metrics.verifiedChanges
    // The cost metrics' REAL denominator: money-settled verified changes. A
    // verified-but-money-pending run leaves it zero and must never render the
    // served folded zero as a measured $0; the exclusion count is surfaced.
    val moneyCompleteDenominator = metrics.verifiedChangesMoneyComplete
    val costGate = verifiedDenominator == 0L || moneyCompleteDenominator == 0L
    val exclusionNote = if (metrics.moneyIncompleteExcluded > 0L) {
        " \u00b7 " + metrics.moneyIncompleteExcluded + " run(s) excluded (money not settled)"
    } else {
        ""
    }
    val totalSpend = MicroMoney.parseDecimal(metrics.totalSpendMicro)
    val noTotalSpend = totalSpend == null || totalSpend.signum() == 0
    val frontierPpm = metrics.frontierEscalationPpm

    rows.add(
        OutcomeMetricRow(
            name = "Successful-Run Cost",
            value = when {
                verifiedDenominator == 0L -> OUTCOMES_NO_VERIFIED_CHANGES
                moneyCompleteDenominator == 0L -> OUTCOMES_NO_MONEY_SETTLED_CHANGES
                else -> moneyText(metrics.successfulRunCostMicro)
            },
            detail = when {
                verifiedDenominator == 0L ->
                    "model + verification spend over 0 verified changes" + exclusionNote
                moneyCompleteDenominator == 0L ->
                    "spend over 0 money-settled verified changes" + exclusionNote
                else ->
                    "model + verification spend over the successful (verified) runs \u00f7 " +
                        moneyCompleteDenominator + " money-settled verified changes" + exclusionNote
            },
            explanation = "The average spend of a run that DID succeed \u2014 failed attempts " +
                "are excluded from this numerator, and a verified run whose money is still " +
                "pending is excluded from the denominator (never a $0 average)."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Verified Cost per Successful Change (VCSC)",
            value = when {
                verifiedDenominator == 0L -> OUTCOMES_NO_VERIFIED_CHANGES
                moneyCompleteDenominator == 0L -> OUTCOMES_NO_MONEY_SETTLED_CHANGES
                else -> moneyText(metrics.verifiedCostPerSuccessfulChangeMicro)
            },
            detail = when {
                verifiedDenominator == 0L ->
                    "TOTAL spend over ALL attempted work \u00f7 0 verified changes" + exclusionNote
                moneyCompleteDenominator == 0L ->
                    "TOTAL spend over ALL attempted work \u00f7 0 money-settled verified " +
                        "changes" + exclusionNote
                else ->
                    "TOTAL spend over ALL attempted work \u00f7 " + moneyCompleteDenominator +
                        " money-settled verified changes" + exclusionNote
            },
            explanation = "All-in cost of one money-settled verified change: 9 failures at $10 " +
                "plus 1 verified change at $1 reads $91 per successful change (never $1)."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Verified First Verification Pass Rate",
            value = if (verifiedDenominator == 0L) {
                OUTCOMES_NO_VERIFIED_CHANGES
            } else {
                ppmPercentText(metrics.verifiedFirstVerificationPassPpm)
            },
            detail = "${metrics.verifiedFirstVerificationPassChanges} of ${metrics.verifiedChanges} verified changes",
            explanation = "verified changes whose FIRST verification attempt passed ÷ verified changes " +
                "(a verification-attempt measure, never an implementation-attempt claim)."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Rework Cost Ratio",
            value = if (noTotalSpend) OUTCOMES_UNAVAILABLE_VALUE else ppmPercentText(metrics.reworkCostRatioPpm),
            detail = if (noTotalSpend) {
                "no model + verification spend recorded; zero denominator (not measured)"
            } else {
                "rework spend " + moneyText(metrics.totalReworkSpendMicro) +
                    " of total spend " + moneyText(metrics.totalSpendMicro)
            },
            explanation = "rework spend \u00f7 (model + verification spend), all runs; an empty " +
                "spend denominator is not measured, never a 0% rate."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Avoided Hosted Inference",
            value = avoidedValue(metrics),
            detail = "sum of recorded avoided_hosted_micro over all runs",
            explanation = "Only explicitly recorded counterfactual values; an unrecorded " +
                "counterfactual stays unmeasured, never a fabricated saving."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Mean Time to Verified Change",
            value = if (verifiedDenominator == 0L) {
                OUTCOMES_NO_VERIFIED_CHANGES
            } else {
                metrics.meanTimeToVerifiedChangeMs.toString() + " ms"
            },
            detail = "wall time over verified changes ÷ verified changes",
            explanation = "Wall time from run start to terminal outcome over the verified " +
                "changes, rounded up."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Human Intervention Rate",
            value = if (metrics.runs == 0L) OUTCOMES_UNAVAILABLE_VALUE else ppmPercentText(metrics.humanInterventionPpm),
            detail = if (metrics.runs == 0L) {
                "no runs recorded; zero denominator (not measured)"
            } else {
                "${metrics.humanInterventionRuns} of ${metrics.runs} runs"
            },
            explanation = "runs with at least one permission decision, manual steer or edit \u00f7 " +
                "all runs; an empty run denominator is not measured, never a 0% rate."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Regression/Rollback Rate",
            value = if (metrics.runs == 0L) OUTCOMES_UNAVAILABLE_VALUE else ppmPercentText(metrics.regressionRollbackPpm),
            detail = if (metrics.runs == 0L) {
                "no runs recorded; zero denominator (not measured)"
            } else {
                "${metrics.rollbackRuns} of ${metrics.runs} runs"
            },
            explanation = "runs with at least one rollback event \u00f7 all runs; an empty run " +
                "denominator is not measured, never a 0% rate."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Proof Coverage",
            value = if (metrics.checksRequired == 0L) {
                OUTCOMES_UNAVAILABLE_VALUE
            } else {
                ppmPercentText(metrics.proofCoveragePpm)
            },
            detail = if (metrics.checksRequired == 0L) {
                "no required checks recorded; zero denominator (not measured)"
            } else {
                "${metrics.checksPassed} of ${metrics.checksRequired} required checks"
            },
            explanation = "passed checks \u00f7 required checks over runs that required any, " +
                "clamped to 100%; no required checks is not measured, never a 0% rate."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Frontier Escalation Rate",
            // A local binding: `frontierEscalationPpm` is a nullable property
            // of another module, so it cannot smart-cast after the null check.
            value = if (frontierPpm == null || metrics.runs == 0L) {
                OUTCOMES_NOT_MEASURED
            } else {
                ppmPercentText(frontierPpm)
            },
            detail = when (metrics.frontierEscalationMeasurement) {
                "unmeasured" ->
                    "no durable execution-class authority records escalations; zero is unmeasured"
                "partial" ->
                    "${metrics.frontierEscalationClassedRuns} of " +
                        "${metrics.frontierClassedRuns} fully-classed runs with a recorded " +
                        "frontier escalation (partial)"
                else ->
                    "${metrics.frontierEscalationRuns} of ${metrics.runs} runs with a recorded " +
                        "frontier escalation (measured)"
            },
            explanation = "runs with at least one recorded frontier escalation \u00f7 runs; null " +
                "(never a fabricated 0%) unless durable execution-class authority exists; " +
                "frontierEscalationMeasurement reports measured | partial | unmeasured, and " +
                "partial covers exactly the fully-classed runs."
        )
    )

    // The distinct local cohorts: local-only / local-final / local-first /
    // fallback stay separate named rows with their own served denominators.
    // An empty denominator renders `—` (never a measured 0% rate).
    rows.add(
        OutcomeMetricRow(
            name = "Local-Only Verified Rate",
            value = if (metrics.localOnlyRuns == 0L) OUTCOMES_UNAVAILABLE_VALUE else ppmPercentText(metrics.localOnlyVerifiedPpm),
            detail = if (metrics.localOnlyRuns == 0L) {
                "no local-only runs recorded"
            } else {
                "${metrics.localOnlyVerifiedChanges} of ${metrics.localOnlyRuns} local-only runs"
            },
            explanation = "Verified local-only runs \u00f7 runs whose model sequence is entirely " +
                "local; no local-only runs is not measured, never a 0% rate."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Local-Final Verified Rate",
            value = if (metrics.localFinalRuns == 0L) OUTCOMES_UNAVAILABLE_VALUE else ppmPercentText(metrics.localFinalVerifiedPpm),
            detail = if (metrics.localFinalRuns == 0L) {
                "no local-final runs recorded"
            } else {
                "${metrics.localFinalVerifiedChanges} of ${metrics.localFinalRuns} local-final runs"
            },
            explanation = "Verified runs whose FINAL model step is local \u00f7 runs whose final " +
                "step is local; no local-final runs is not measured, never a 0% rate."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Local-First Success Rate",
            value = if (metrics.localFirstRuns == 0L) OUTCOMES_UNAVAILABLE_VALUE else ppmPercentText(metrics.localFirstSuccessPpm),
            detail = if (metrics.localFirstRuns == 0L) {
                "no local-first runs recorded"
            } else {
                "${metrics.localFirstSuccessChanges} of ${metrics.localFirstRuns} local-first runs"
            },
            explanation = "Runs whose FIRST step is durably local, whose FINAL step is ALSO " +
                "durably local, and whose FIRST verification attempt passed \u00f7 runs whose " +
                "FIRST step is durably local; no local-first runs is not measured."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Local-to-Hosted Fallback Rate",
            value = if (metrics.localFirstRuns == 0L) OUTCOMES_UNAVAILABLE_VALUE else ppmPercentText(metrics.localToHostedFallbackPpm),
            detail = if (metrics.localFirstRuns == 0L) {
                "no local-first runs recorded"
            } else {
                "${metrics.localToHostedFallbackRuns} of ${metrics.localFirstRuns} local-first runs"
            },
            explanation = "Runs that started on a durably local step and ENDED on a durably " +
                "hosted step \u00f7 runs whose first step is durably local; a local-then-hosted " +
                "repair is a fallback, never a local substitution."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Hosted-Only Rate",
            // The SERVED denominator is runs WITH model steps (a stepless run
            // carries no hosted evidence), so the row gates on it — never on
            // all runs.
            value = if (metrics.runsWithSteps == 0L) OUTCOMES_UNAVAILABLE_VALUE else ppmPercentText(metrics.hostedOnlyPpm),
            detail = if (metrics.runsWithSteps == 0L) {
                "no runs with model steps recorded"
            } else {
                "${metrics.hostedOnlyRuns} of ${metrics.runsWithSteps} runs with model steps " +
                    "whose every step is durably hosted"
            },
            explanation = "Runs whose every step is durably HOSTED \u00f7 runs with steps; an " +
                "unknown execution class is never counted as local or hosted, and no runs with " +
                "steps is not measured, never a 0% rate."
        )
    )

    val runRows = page.runs.take(OUTCOMES_MAX_RUN_ROWS).map { run ->
        OutcomeRunRow(
            runId = run.runId,
            taskClass = run.taskClass,
            riskClass = run.riskClass,
            verified = run.verified,
            firstVerificationPass = run.verifiedFirstVerificationPass,
            modelSpendMicro = run.modelSpendMicro,
            reworkSpendMicro = run.reworkSpendMicro,
            wallTimeMs = run.wallTimeMs,
            completedAtMs = run.completedAtMs
        )
    }
    return OutcomesPanelModel(
        state = "ok",
        reason = null,
        metrics = metrics,
        metricRows = rows,
        runRows = runRows,
        nextCursor = page.nextCursor,
        scanned = metrics.scanned,
        truncated = metrics.truncated,
        pagingExhausted = page.nextCursor == null,
        refusalCode = null,
        entitlementsStatus = entitlementsStatus
    )
}

/** The TYPED avoided-hosted fact: `unavailable` (or a null value) is not
 * measured; a MEASURED value — including a measured zero — is exact. */
private fun avoidedValue(metrics: ProtocolOutcomeMetrics): String {
    if (metrics.avoidedHostedInferenceMeasurement != "measured") return OUTCOMES_NOT_MEASURED
    val raw = metrics.avoidedHostedInferenceMicro ?: return OUTCOMES_NOT_MEASURED
    if (MicroMoney.parseDecimal(raw) == null) return OUTCOMES_NOT_MEASURED
    return moneyText(raw)
}

/** Guard: every served money value of the metrics fold parses exactly. */
private fun moneyGlyph(metrics: ProtocolOutcomeMetrics): BigInteger? {
    val values = listOf(
        metrics.totalVerifiedSpendMicro,
        metrics.successfulRunCostMicro,
        metrics.totalSpendMicro,
        metrics.verifiedCostPerSuccessfulChangeMicro,
        metrics.totalReworkSpendMicro
    ) + listOfNotNull(metrics.avoidedHostedInferenceMicro)
    for (value in values) {
        if (MicroMoney.parseDecimal(value) == null) return null
    }
    return BigInteger.ZERO
}

/** Exact micro-USD display: `$1234.5678 · 1234567890µ$` (never scientific). */
internal fun moneyText(micro: String): String {
    val value = MicroMoney.parseDecimal(micro) ?: return OUTCOMES_NOT_MEASURED
    return "$" + FormattedValues.usd(value) + " \u00b7 " + FormattedValues.micro(value)
}

/**
 * Exact floor-ppm percent with four decimals (`333333` ppm -> `33.3333%`).
 * Rates are floor ppm ("at least"), so the display never rounds up.
 */
internal fun ppmPercentText(ppm: Long): String {
    if (ppm < 0L) return OUTCOMES_NOT_MEASURED
    val whole = ppm / 10_000L
    val fraction = (ppm % 10_000L).toString().padStart(4, '0')
    return whole.toString() + "." + fraction + "%"
}

class OutcomesPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onRefresh()

        fun onOlderPage()

        /** T29 `View details`: opens the Diagnostics utility. */
        fun onViewDetails()
    }

    private val statusLabel = WrappedLabel("outcomes: -")

    /**
     * The value-entitlements status line of the configured control-plane
     * organization (hidden while no organization is configured).
     */
    private val entitlementsLabel = wrappedMutedLabel("")

    private val truncatedLabel = WrappedLabel("")

    private val metricsColumn = ScrollableColumn()

    private val cohortColumn = ScrollableColumn()

    private val runsColumn = ScrollableColumn()

    private val runsNote = wrappedMutedLabel("")

    private val explainToggle = secondaryButton("Explain metric")

    private val refreshButton = secondaryButton("Refresh")

    private val olderButton = secondaryButton("Older runs")

    private val unavailableHeadline = sectionHeader(OUTCOMES_COST_UNAVAILABLE)

    private val unavailableCopy = wrappedMutedLabel(OUTCOMES_COST_UNAVAILABLE_COPY)

    private val unavailableReason = wrappedMutedLabel("")

    private val viewDetailsButton = secondaryButton("View details")

    private val unavailableCard = JPanel(BorderLayout())

    private val contentHost = JPanel(java.awt.CardLayout())

    private val contentLayout = contentHost.layout as java.awt.CardLayout

    private var listener: Listener? = null

    private var model: OutcomesPanelModel? = null

    private var applied = false

    private var explainExpanded = false

    init {
        metricsColumn.layout = BoxLayout(metricsColumn, BoxLayout.Y_AXIS)
        cohortColumn.layout = BoxLayout(cohortColumn, BoxLayout.Y_AXIS)
        runsColumn.layout = BoxLayout(runsColumn, BoxLayout.Y_AXIS)
        explainToggle.addActionListener {
            explainExpanded = !explainExpanded
            explainToggle.text = if (explainExpanded) "Hide explanations" else "Explain metric"
            renderMetrics(model)
            revalidate()
            repaint()
        }
        refreshButton.addActionListener { listener?.onRefresh() }
        olderButton.addActionListener { listener?.onOlderPage() }
        viewDetailsButton.addActionListener { listener?.onViewDetails() }
        val metricsBody = pageColumn(gap = Spacing.XS, padding = 0)
        metricsBody.add(truncatedLabel)
        metricsBody.add(metricsColumn)
        metricsBody.add(actionRow(explainToggle, refreshButton))
        val cohortBody = pageColumn(gap = Spacing.XS, padding = 0)
        cohortBody.add(cohortColumn)
        val runsBody = pageColumn(gap = Spacing.XS, padding = 0)
        runsBody.add(runsColumn)
        runsBody.add(runsNote)
        runsBody.add(actionRow(olderButton))
        val content = pageColumn()
        content.add(card("Outcomes", metricsBody))
        content.add(vSpace(Spacing.M))
        content.add(card("Local cohorts", cohortBody))
        content.add(vSpace(Spacing.M))
        content.add(card("Recent runs", runsBody))
        val unavailableBody = pageColumn(gap = Spacing.XS, padding = 0)
        unavailableBody.add(unavailableHeadline)
        unavailableBody.add(unavailableCopy)
        unavailableBody.add(unavailableReason)
        unavailableBody.add(vSpace(Spacing.XS))
        unavailableBody.add(actionRow(viewDetailsButton, refreshButton))
        unavailableCard.isOpaque = false
        unavailableCard.add(unavailableBody, BorderLayout.NORTH)
        val contentScroll = pageScroll(content)
        contentHost.isOpaque = false
        contentHost.add(contentScroll, "content")
        contentHost.add(pageScroll(unavailableCard), "unavailable")
        val header = pageColumn(gap = Spacing.XS, padding = 0)
        header.add(panelHeader(statusLabel))
        header.add(entitlementsLabel)
        add(header, BorderLayout.NORTH)
        add(contentHost, BorderLayout.CENTER)
        setModel(null)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Applies one model (null = nothing read yet). */
    fun setModel(value: OutcomesPanelModel?) {
        if (applied && model == value) return
        applied = true
        model = value
        when (value?.state) {
            "ok" -> {
                contentLayout.show(contentHost, "content")
                renderMetrics(value)
                renderCohorts(value)
                renderRuns(value)
            }
            "unavailable" -> {
                contentLayout.show(contentHost, "unavailable")
                unavailableReason.fullText = value.reason ?: ""
                unavailableReason.foreground = mutedForeground()
            }
            else -> {
                contentLayout.show(contentHost, "content")
                renderMetrics(null)
                renderCohorts(null)
                renderRuns(null)
            }
        }
        statusLabel.text = when {
            value == null -> "outcomes: not read yet"
            value.state == "ok" -> "outcomes: " + plural(value.metricRows.size, "metric") +
                " · " + plural(value.runRows.size, "run") +
                if (value.truncated) " · [TRUNCATED]" else ""
            value.state == "unavailable" -> "outcomes: cost unavailable" +
                (value.refusalCode?.let { " [" + it + "]" } ?: "")
            else -> "outcomes: not read yet"
        }
        statusLabel.foreground = if (value?.state == "unavailable") {
            semanticForeground(SemanticState.NEGATIVE)
        } else {
            textForeground()
        }
        val entitlementsText = value?.entitlementsStatus ?: ""
        entitlementsLabel.fullText = entitlementsText
        entitlementsLabel.isVisible = entitlementsText.isNotEmpty()
        revalidate()
        repaint()
    }

    private fun renderMetrics(value: OutcomesPanelModel?) {
        metricsColumn.removeAll()
        truncatedLabel.fullText = when {
            value == null -> ""
            value.state == "unavailable" -> ""
            value.truncated ->
                "[TRUNCATED] the fold scanned the newest ${value.scanned} record(s); older " +
                    "records are excluded from every metric"
            else -> "the fold scanned ${value.scanned} record(s) (no truncation)"
        }
        for (row in value?.metricRows.orEmpty()) {
            val body = pageColumn(gap = Spacing.XS, padding = 0)
            body.add(sectionHeader(row.name, muted = false))
            val valueLabel = WrappedLabel(row.value)
            valueLabel.font = sectionTitleFont()
            body.add(valueLabel)
            body.add(wrappedMutedLabel(row.detail))
            if (explainExpanded) {
                body.add(wrappedMutedLabel(row.explanation))
            }
            metricsColumn.add(card(null, body))
            metricsColumn.add(vSpace(Spacing.S))
        }
        metricsColumn.revalidate()
        metricsColumn.repaint()
    }

    /**
     * The local cohort card shows the cohort MEMBERSHIP lines (counts that
     * distinguish local-only from local-final from local-first from
     * fallback) next to the metric rows above.
     */
    private fun renderCohorts(value: OutcomesPanelModel?) {
        cohortColumn.removeAll()
        val metrics = value?.metrics
        if (metrics == null) {
            cohortColumn.add(mutedLabel("No local-cohort counters are served yet."))
        } else {
            cohortColumn.add(
                wrappedMutedLabel(
                    "local runs (at least one local step): " + metrics.localRuns +
                        " of " + metrics.runs + " runs"
                )
            )
            cohortColumn.add(
                wrappedMutedLabel(
                    "local-only runs (every step local): " + metrics.localOnlyRuns
                )
            )
            cohortColumn.add(
                wrappedMutedLabel("local-final runs (final step local): " + metrics.localFinalRuns)
            )
            cohortColumn.add(
                wrappedMutedLabel("local-first runs (first step local): " + metrics.localFirstRuns)
            )
            cohortColumn.add(
                wrappedMutedLabel(
                    "local-to-hosted fallback runs: " + metrics.localToHostedFallbackRuns
                )
            )
            cohortColumn.add(
                wrappedMutedLabel("hosted-only runs (every step durably hosted): " + metrics.hostedOnlyRuns)
            )
            cohortColumn.add(
                wrappedMutedLabel(
                    "runs with model steps (Hosted-Only Rate denominator): " + metrics.runsWithSteps
                )
            )
            cohortColumn.add(
                wrappedMutedLabel(
                    "money-incomplete runs excluded from cost metrics: " +
                        metrics.moneyIncompleteExcluded
                )
            )
        }
        cohortColumn.revalidate()
        cohortColumn.repaint()
    }

    private fun renderRuns(value: OutcomesPanelModel?) {
        runsColumn.removeAll()
        val rows = value?.runRows.orEmpty()
        if (rows.isEmpty()) {
            runsColumn.add(mutedLabel("No completed runs are served on this page yet."))
        } else {
            for (row in rows) {
                val verification = if (row.verified) "verified" else "not verified"
                val attempt = if (row.firstVerificationPass) "first verification pass" else "after repair"
                val modelSpend = row.modelSpendMicro?.let { moneyText(it) } ?: OUTCOMES_NOT_MEASURED
                val reworkSpend = row.reworkSpendMicro?.let { moneyText(it) } ?: OUTCOMES_NOT_MEASURED
                runsColumn.add(
                    wrappedMutedLabel(
                        row.runId + " \u00b7 " + row.taskClass + "/" + row.riskClass + " \u00b7 " +
                            verification + " (" + attempt + ") \u00b7 model spend " +
                            modelSpend + " \u00b7 rework " +
                            reworkSpend + " \u00b7 " + row.wallTimeMs + " ms"
                    )
                )
            }
        }
        runsNote.fullText = when {
            value == null -> ""
            value.nextCursor != null ->
                "bounded newest-first page; older runs continue from the composite cursor " +
                    "(completedAtMs=" + value.nextCursor.completedAtMs + ", run=" + value.nextCursor.runId + ")"
            else -> "the served run page is exhausted (no older cursor)"
        }
        olderButton.isEnabled = value?.olderEnabled() == true
        runsColumn.revalidate()
        runsColumn.repaint()
    }

    // ---------------------------------------------------------- observables

    fun model(): OutcomesPanelModel? = model

    fun metricNames(): List<String> = model?.metricRows?.map { it.name } ?: emptyList()

    fun metricValue(name: String): String? =
        model?.metricRows?.firstOrNull { it.name == name }?.value

    fun olderEnabled(): Boolean = olderButton.isEnabled

    fun explainExpanded(): Boolean = explainExpanded

    internal fun clickExplain() {
        explainToggle.doClick()
    }

    internal fun clickOlder() {
        olderButton.doClick()
    }

    internal fun clickViewDetails() {
        viewDetailsButton.doClick()
    }

    internal fun clickRefresh() {
        refreshButton.doClick()
    }

    fun unavailableHeadlineText(): String = unavailableHeadline.fullText

    fun unavailableCopyText(): String = unavailableCopy.fullText

    fun viewDetailsText(): String = viewDetailsButton.text

    fun truncatedText(): String = truncatedLabel.fullText

    /** The rendered value-entitlements status line ("" while hidden). */
    fun entitlementsStatusText(): String = entitlementsLabel.fullText

    /** The rendered status line (typed refusal codes surface here). */
    fun statusText(): String = statusLabel.text
}
