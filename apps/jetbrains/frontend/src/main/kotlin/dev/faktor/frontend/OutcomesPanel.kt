// I07 — the Engineering Outcome Ledger surface (GET /native/outcomes).
//
// Pure projection: [OutcomesPanelModel] carries every rendered fact (exact
// metric names/definitions from docs/outcome-ledger.md, served counts and
// denominators, scan honesty) so the smoke drives it without a display; the
// Swing panel renders it. Money stays a BigInteger end-to-end (MicroMoney
// helpers, never scientific notation); an empty verified denominator renders
// the exact `No verified changes yet`, an unmeasured counterfactual renders
// `not measured` (never `0` as a savings/free claim), and a read failure is
// the T29 `Cost unavailable` state with no monetary value at all.
package dev.faktor.frontend

import dev.faktor.shared.MicroMoney
import dev.faktor.shared.ProtocolOutcomeCursor
import dev.faktor.shared.ProtocolOutcomeMetrics
import dev.faktor.shared.ProtocolOutcomesPage
import dev.faktor.shared.ProtocolRunOutcome
import java.awt.BorderLayout
import java.math.BigInteger
import javax.swing.BoxLayout
import javax.swing.JPanel

/** Exact empty-denominator copy (never a zero rate claim). */
const val OUTCOMES_NO_VERIFIED_CHANGES = "No verified changes yet"

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
    val firstAttempt: Boolean,
    val modelSpendMicro: String,
    val reworkSpendMicro: String,
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
    val pagingExhausted: Boolean
) {
    fun olderEnabled(): Boolean = nextCursor != null

    fun hasMonetaryValue(): Boolean = metricRows.any { it.value.startsWith("$") }

    fun metricNames(): List<String> = metricRows.map { it.name }

    fun metricValue(name: String): String? = metricRows.firstOrNull { it.name == name }?.value

    /** The bounded display lines (smoke-visible; exact metric names). */
    fun lines(): List<String> = metricRows.map { "${it.name}: ${it.value} — ${it.detail}" } +
        runRows.map { "run ${it.runId} [$it.taskClass/$it.riskClass] verified=${it.verified}" }
}

/**
 * Build the panel model from one served page plus the last refusal. A null
 * page without a refusal is the honest `not_read` state (nothing invented);
 * a refusal is the T29 `unavailable` state with NO monetary rows.
 */
fun outcomesPanelModelOf(
    page: ProtocolOutcomesPage?,
    refusalReason: String? = null
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
            pagingExhausted = false
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
            pagingExhausted = page.nextCursor == null
        )
    }
    val rows = ArrayList<OutcomeMetricRow>()
    val verifiedDenominator = metrics.verifiedChanges

    rows.add(
        OutcomeMetricRow(
            name = "Successful-Run Cost",
            value = if (verifiedDenominator == 0L) {
                OUTCOMES_NO_VERIFIED_CHANGES
            } else {
                moneyText(metrics.successfulRunCostMicro)
            },
            detail = "model + verification spend over the successful (verified) runs ÷ verified changes",
            explanation = "The average spend of a run that DID succeed — failed attempts are " +
                "excluded from this numerator."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Verified Cost per Successful Change (VCSC)",
            value = if (verifiedDenominator == 0L) {
                OUTCOMES_NO_VERIFIED_CHANGES
            } else {
                moneyText(metrics.verifiedCostPerSuccessfulChangeMicro)
            },
            detail = "TOTAL spend over ALL attempted work ÷ verified changes",
            explanation = "All-in cost of one verified change: 9 failures at $10 plus 1 verified " +
                "change at $1 reads $91 per successful change (never $1)."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Verified First-Pass Rate",
            value = if (verifiedDenominator == 0L) {
                OUTCOMES_NO_VERIFIED_CHANGES
            } else {
                ppmPercentText(metrics.verifiedFirstPassPpm)
            },
            detail = "${metrics.verifiedFirstPassChanges} of ${metrics.verifiedChanges} verified changes",
            explanation = "first-attempt verified changes ÷ verified changes."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Rework Cost Ratio",
            value = ppmPercentText(metrics.reworkCostRatioPpm),
            detail = "rework spend " + moneyText(metrics.totalReworkSpendMicro) +
                " of total spend " + moneyText(metrics.totalSpendMicro),
            explanation = "rework spend ÷ (model + verification spend), all runs."
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
            value = ppmPercentText(metrics.humanInterventionPpm),
            detail = "${metrics.humanInterventionRuns} of ${metrics.runs} runs",
            explanation = "runs with at least one permission decision, manual steer or edit ÷ all runs."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Regression/Rollback Rate",
            value = ppmPercentText(metrics.regressionRollbackPpm),
            detail = "${metrics.rollbackRuns} of ${metrics.runs} runs",
            explanation = "runs with at least one rollback event ÷ all runs."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Proof Coverage",
            value = if (metrics.checksRequired == 0L) {
                "no required checks"
            } else {
                ppmPercentText(metrics.proofCoveragePpm)
            },
            detail = "${metrics.checksPassed} of ${metrics.checksRequired} required checks",
            explanation = "passed checks ÷ required checks over runs that required any, clamped to 100%."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Frontier Escalation Rate",
            value = ppmPercentText(metrics.frontierEscalationPpm),
            detail = "${metrics.frontierEscalationRuns} of ${metrics.runs} runs",
            explanation = "runs with at least one recorded frontier escalation ÷ all runs; a hosted " +
                "step alone is never inferred as a frontier escalation."
        )
    )

    // The distinct local cohorts: local-only / local-final / local-first /
    // fallback stay separate named rows with their own served denominators.
    rows.add(
        OutcomeMetricRow(
            name = "Local-Only Verified Rate",
            value = ppmPercentText(metrics.localOnlyVerifiedPpm),
            detail = "${metrics.localOnlyVerifiedChanges} of ${metrics.localOnlyRuns} local-only runs",
            explanation = "Verified local-only runs ÷ runs whose model sequence is entirely local."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Local-Final Verified Rate",
            value = ppmPercentText(metrics.localFinalVerifiedPpm),
            detail = "${metrics.localFinalVerifiedChanges} of ${metrics.localFinalRuns} local-final runs",
            explanation = "Verified runs whose FINAL model step is local ÷ runs whose final step is local."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Local-First Success Rate",
            value = ppmPercentText(metrics.localFirstSuccessPpm),
            detail = "${metrics.localFirstSuccessChanges} of ${metrics.localFirstRuns} local-first runs",
            explanation = "Local-first runs that verified on their first attempt ÷ runs whose FIRST step is local."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Local-to-Frontier Fallback Rate",
            value = ppmPercentText(metrics.localToFrontierFallbackPpm),
            detail = "${metrics.localToFrontierFallbackRuns} of ${metrics.localFirstRuns} local-first runs",
            explanation = "Runs that started local and ENDED hosted ÷ runs whose first step is local; " +
                "a local-then-hosted repair is a fallback, never a local substitution."
        )
    )
    rows.add(
        OutcomeMetricRow(
            name = "Frontier-Only Rate",
            value = ppmPercentText(metrics.frontierOnlyPpm),
            detail = "${metrics.frontierOnlyRuns} of ${metrics.runs} runs",
            explanation = "Runs with no local step at all ÷ all runs."
        )
    )

    val runRows = page.runs.take(OUTCOMES_MAX_RUN_ROWS).map { run ->
        OutcomeRunRow(
            runId = run.runId,
            taskClass = run.taskClass,
            riskClass = run.riskClass,
            verified = run.verified,
            firstAttempt = run.verifiedFirstAttempt,
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
        pagingExhausted = page.nextCursor == null
    )
}

/** `0` avoided hosted inference is unmeasured, never a free-inference claim. */
private fun avoidedValue(metrics: ProtocolOutcomeMetrics): String {
    val parsed = MicroMoney.parseDecimal(metrics.avoidedHostedInferenceMicro)
        ?: return OUTCOMES_NOT_MEASURED
    if (parsed.signum() == 0) return OUTCOMES_NOT_MEASURED
    return moneyText(metrics.avoidedHostedInferenceMicro)
}

/** Guard: every served money value of the metrics fold parses exactly. */
private fun moneyGlyph(metrics: ProtocolOutcomeMetrics): BigInteger? {
    val values = listOf(
        metrics.totalVerifiedSpendMicro,
        metrics.successfulRunCostMicro,
        metrics.totalSpendMicro,
        metrics.verifiedCostPerSuccessfulChangeMicro,
        metrics.totalReworkSpendMicro,
        metrics.avoidedHostedInferenceMicro
    )
    for (value in values) {
        if (MicroMoney.parseDecimal(value) == null) return null
    }
    return BigInteger.ZERO
}

/** Exact micro-USD display: `$1234.5678 · 1234567890µ$` (never scientific). */
internal fun moneyText(micro: String): String {
    val value = MicroMoney.parseDecimal(micro) ?: return OUTCOMES_NOT_MEASURED
    return "$" + MicroMoney.usdText(value) + " \u00b7 " + value.toString() + "\u00b5\$"
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
        add(panelHeader(statusLabel), BorderLayout.NORTH)
        add(contentHost, BorderLayout.CENTER)
        setModel(null)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Applies one model (null = nothing read yet). */
    fun setModel(value: OutcomesPanelModel?) {
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
        statusLabel.text = when (value?.state) {
            "ok" -> "outcomes: " + plural(value.metricRows.size, "metric") +
                " · " + plural(value.runRows.size, "run") +
                if (value.truncated) " · [TRUNCATED]" else ""
            "unavailable" -> "outcomes: cost unavailable"
            else -> "outcomes: not read yet"
        }
        statusLabel.foreground = if (value?.state == "unavailable") {
            semanticForeground(SemanticState.NEGATIVE)
        } else {
            textForeground()
        }
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
                    "local-to-frontier fallback runs: " + metrics.localToFrontierFallbackRuns
                )
            )
            cohortColumn.add(
                wrappedMutedLabel("frontier-only runs (no local step): " + metrics.frontierOnlyRuns)
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
                val attempt = if (row.firstAttempt) "first attempt" else "after repair"
                runsColumn.add(
                    wrappedMutedLabel(
                        row.runId + " \u00b7 " + row.taskClass + "/" + row.riskClass + " \u00b7 " +
                            verification + " (" + attempt + ") \u00b7 model spend " +
                            moneyText(row.modelSpendMicro) + " \u00b7 rework " +
                            moneyText(row.reworkSpendMicro) + " \u00b7 " + row.wallTimeMs + " ms"
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
}
