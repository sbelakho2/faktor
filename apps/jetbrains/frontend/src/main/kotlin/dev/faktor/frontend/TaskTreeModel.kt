// Pure view-model construction for the rich task-tree panel. Everything
// here is display-independent (no Swing types except the deterministic
// pixel avatars) so the smoke can build and assert EVERY section from a
// canned native payload without a display, and the panel classes only map
// this model onto components.
//
// Sources are the native surfaces already served by the daemon:
//   GET /session/{id}/projection          -> goal/state/phase fallback
//   GET /native/session/{id}/tasks        -> task view (acceptance/plan/...)
//   GET /native/agents                    -> children + blockers + result
//   GET /native/orchestrator/graph        -> durable plan step/DAG order
//   GET /models                           -> provider/reasoning metadata
//   GET /native/session/{id}/verification -> owed/failed checks
//   GET .../tasks/{task_id}/verification  -> criteria + evidence refs
//   GET /native/session/{id}/usage        -> durable spend
//   GET /native/session/{id}/tournament/{tid} -> candidates/winner state
package dev.faktor.frontend

import dev.faktor.shared.NativeAgent
import dev.faktor.shared.NativeAgentProgress
import dev.faktor.shared.NativeBlocker
import dev.faktor.shared.NativeChildResult
import dev.faktor.shared.NativeCompletionContract
import dev.faktor.shared.NativeCriterionBinding
import dev.faktor.shared.NativeCriterionVerdict
import dev.faktor.shared.NativeModelInfo
import dev.faktor.shared.NativeOrchestratorGraph
import dev.faktor.shared.NativeProjection
import dev.faktor.shared.NativeSessionUsage
import dev.faktor.shared.NativeTaskProof
import dev.faktor.shared.NativeTaskVerification
import dev.faktor.shared.NativeTaskView
import dev.faktor.shared.NativeTournament
import dev.faktor.shared.NativeTournamentCandidate
import dev.faktor.shared.NativeTournamentCriterion
import dev.faktor.shared.NativeTournamentSummary
import dev.faktor.shared.NativeVerificationRecord
import dev.faktor.shared.NativeVerificationView
import java.math.BigInteger

/** Max evidence refs surfaced by one task tree (bounded like the cockpit). */
const val MAX_TREE_EVIDENCE = 64

/** Max acceptance-criterion proof rows one task tree renders (bounded). */
const val MAX_CRITERIA_PROOF_ROWS = 32

/** Max label chars of one evidence ref (free text is kept but bounded). */
const val MAX_EVIDENCE_LABEL = 300

/** The visual tone of one criterion verdict (unavailable is distinct). */
enum class CriterionVerdictTone {
    PASS,
    FAIL,
    UNAVAILABLE
}

/**
 * One acceptance-criterion PROOF row: why Faktor believes the criterion is
 * complete. Built from the members the shared task-verification DTO serves:
 * the criterion key, the recorded boolean verdict, the evidence string, the
 * TYPED binding (kind + its own members), the served three-way verdict and
 * the record-level proof snapshots/timestamps. A member the serving daemon
 * predates stays an explicit unavailable marker that names the missing
 * member — never a guessed value and never a pass.
 */
data class CriterionProofRow(
    val criterionKey: String,
    /** required | preferred | unavailable plus the missing DTO field. */
    val requirement: String,
    val origin: String,
    /** required_check | integration_coverage | file_state | evidence |
     *  independent_review | aggregate_goal | unavailable. */
    val bindingKind: String,
    /** daemon | derived | unavailable. */
    val bindingSource: String,
    /** The exact check/file/work-item/evidence reference, when identifiable. */
    val bindingReference: String?,
    val bindingDetail: String?,
    /** pass | fail | unavailable — an unavailable row NEVER renders as pass. */
    val verdict: String,
    val verdictReason: String?,
    val evidenceRefs: List<EvidenceRef>,
    /** The proven snapshots (explicit unavailable text on a legacy wire). */
    val snapshot: String,
    /** The verification timestamps (explicit unavailable text on a legacy wire). */
    val verificationTimestamp: String,
    val recordId: String?,
    val recordStatus: String?,
    /** Names of the proof members this client's DTO genuinely does not carry. */
    val unavailable: List<String>
) {
    val tone: CriterionVerdictTone
        get() = when (verdict) {
            "pass" -> CriterionVerdictTone.PASS
            "fail" -> CriterionVerdictTone.FAIL
            else -> CriterionVerdictTone.UNAVAILABLE
        }
}

/** A blocker action that applies to one blocked child. */
enum class BlockerAction(val label: String) {
    RESUME("Resume"),
    RETRY("Retry"),
    PERMISSION_ALLOW("Allow"),
    PERMISSION_DENY("Deny"),
    CANCEL("Cancel")
}

/** One evidence reference: `evidence:41` / `evidence/41` / `#41` or free text. */
data class EvidenceRef(val id: Long?, val label: String)

/** `evidence:<n>` ref parsing, mirroring the VS Code cockpit vocabulary. */
object EvidenceRefs {
    private val PATTERN = Regex("(?:^|[^A-Za-z0-9_])evidence[:#/]([0-9]+)(?![0-9])")

    fun parse(raw: String): EvidenceRef {
        val bounded = raw.trim().take(MAX_EVIDENCE_LABEL)
        val match = PATTERN.find(bounded)
        val id = if (match == null) null else match.groupValues[1].toLongOrNull()
        return EvidenceRef(id, bounded)
    }
}

/** Bounded digest label so proof facts survive the panel clamp. */
internal fun shortDigest(digest: String): String =
    if (digest.length <= 16) digest else digest.substring(0, 12) + "..."

/** One blocked child with the actions that apply to its blocker kind. */
data class BlockerRow(
    val childId: String,
    val kind: String,
    val reason: String,
    val dependency: String?,
    val resolution: String?,
    val lastProgressMs: Long?,
    val presence: PixelPresence,
    val actions: List<BlockerAction>
)

/** One plan/DAG step with the children currently driving it. */
data class StepNode(
    val id: String,
    val summary: String,
    val state: String,
    val dependsOn: List<String>,
    val childIds: List<String>
)

/** One child agent with every native field the listing serves. */
data class ChildNode(
    val childId: String,
    val runId: String,
    val sessionId: Long,
    val itemId: String?,
    val itemKind: String?,
    val goal: String,
    val state: String,
    val presence: PixelPresence,
    val blocker: NativeBlocker?,
    val model: String?,
    val provider: String?,
    val reasoning: Boolean?,
    val tools: Boolean?,
    val ownership: String,
    val worktreeId: Long,
    val budgetMaxTokens: Long?,
    val spentTokens: Long?,
    /** Exact micro-USD (BigInteger; never a rounded float). */
    val spentCostMicro: BigInteger?,
    val maxCostMicro: BigInteger?,
    val remainingTokens: Long?,
    val remainingCostMicro: BigInteger?,
    val progress: NativeAgentProgress?,
    val result: NativeChildResult?,
    /** Durable presentation/attention state: foreground or background. */
    val presentation: String = "foreground"
) {
    /** Background children render dimmed and are tucked after the foreground ones. */
    val background: Boolean
        get() = presentation == "background"
}

/** Verification status of the task tree (criteria/checks/owed). */
data class VerificationSummary(
    val status: String,
    val criteriaPassed: Int,
    val criteriaTotal: Int,
    val failedChecks: Int,
    val owed: Int,
    val recordStatus: String?,
    /**
     * True only when a served record proves the FULL criterion set with no
     * failed check: the top-level summary may then render VERIFIED. A
     * missing record is never verified.
     */
    val verified: Boolean = false,
    val reviewer: String? = null,
    val tree: String? = null,
    val publishedCommit: String? = null,
    val remotePrHead: String? = null
) {
    /**
     * The bounded top-level summary line: VERIFIED plus the proof facts the
     * daemon serves (criteria/checks/review/tree/published commit/remote PR
     * head); otherwise the plain status. Cost is appended by the panel from
     * the durable spend block.
     */
    fun summaryText(): String {
        if (!verified) return status
        val bits = ArrayList<String>()
        bits.add("VERIFIED")
        bits.add("criteria " + criteriaPassed + "/" + criteriaTotal)
        bits.add("checks failed " + failedChecks)
        reviewer?.takeIf { it.isNotEmpty() }?.let { bits.add("review " + it) }
        tree?.takeIf { it.isNotEmpty() }?.let { bits.add("tree " + shortDigest(it)) }
        publishedCommit?.takeIf { it.isNotEmpty() }?.let { bits.add("commit " + shortDigest(it)) }
        remotePrHead?.takeIf { it.isNotEmpty() }?.let { bits.add("head " + shortDigest(it)) }
        return bits.joinToString(" · ")
    }
}

/** One completion-contract step as the tree renders it. */
data class CompletionStepView(
    val step: String,
    val status: String,
    val detail: String?
)

/**
 * The Task-mode completion contract + per-step statuses. `source` names the
 * provenance of the rows:
 *  - `daemon`      — served by the daemon;
 *  - `derived`     — projected from the durable task-run state and the
 *                    submitted contract (pending, or all-succeeded only
 *                    because the durable gate certified the task);
 *  - `unavailable` — contract known but this daemon exposes no per-step
 *                    read (terminal non-certified run). Never fabricated.
 */
data class CompletionView(
    val includeCommit: Boolean,
    val includePush: Boolean,
    val includePr: Boolean,
    val steps: List<CompletionStepView>,
    val source: String,
    val reason: String?
)

/** One durable completion step of the proof drill-down. */
data class ProofStepView(
    val step: String,
    val status: String,
    val present: Boolean,
    val detail: String?,
    val snapshot: String?,
    val atMs: Long?
)

/** One explicitly unavailable proof component (missing/unavailable/corrupt/mismatch). */
data class ProofUnavailableView(
    val component: String,
    val kind: String,
    val reason: String
)

/**
 * The top-level VERIFIED view derived from `GET /native/tasks/{id}/proof`
 * (strict payload, daemon verdict preserved verbatim). `verified` is true
 * ONLY for `state == "verified"`; a failed read is `unavailable` with the
 * refusal text — a stale or fabricated VERIFIED can never render.
 */
data class ProofView(
    val state: String,
    val reason: String?,
    val criteriaPassed: Int,
    val criteriaTotal: Int,
    val checksPassed: Int,
    val checksTotal: Int,
    val requiredPassed: Int,
    val requiredTotal: Int,
    val reviewStatus: String?,
    val reviewer: String?,
    val independentVerdict: String,
    val verifiedSnapshot: String?,
    val landedSnapshot: String?,
    val landedEqualsVerified: Boolean?,
    val commitOid: String?,
    val remoteHeadOid: String?,
    val pullRequestId: String?,
    val costStatus: String,
    val spentCostMicro: BigInteger?,
    val maxCostMicro: BigInteger?,
    val costReason: String?,
    val gateStatus: String?,
    val gateReason: String?,
    val steps: List<ProofStepView>,
    val unavailable: List<ProofUnavailableView>
) {
    val verified: Boolean get() = state == "verified"

    /** The bounded top-level line (VERIFIED only for the verified verdict). */
    fun summaryText(): String {
        val head = when (state) {
            "verified" -> "VERIFIED"
            "unavailable" -> "VERIFICATION UNAVAILABLE"
            else -> "NOT VERIFIED"
        }
        val bits = ArrayList<String>()
        bits.add("criteria " + criteriaPassed + "/" + criteriaTotal)
        bits.add("checks " + checksPassed + "/" + checksTotal + " (required " + requiredPassed + "/" + requiredTotal + ")")
        bits.add("review " + (reviewStatus ?: "none"))
        bits.add(
            "verified==landed: " + when (landedEqualsVerified) {
                true -> "yes"
                false -> "NO"
                null -> "unknown"
            }
        )
        commitOid?.takeIf { it.isNotEmpty() }?.let { bits.add("commit " + shortDigest(it)) }
        remoteHeadOid?.takeIf { it.isNotEmpty() }?.let { bits.add("remote head " + shortDigest(it)) }
        pullRequestId?.takeIf { it.isNotEmpty() }?.let { bits.add("PR " + it) }
        if (costStatus == "known") {
            bits.add(
                "spend " + (spentCostMicro ?: BigInteger.ZERO) + "micro" +
                    (maxCostMicro?.let { " of " + it + "micro" } ?: "")
            )
        } else {
            bits.add("spend unavailable" + (costReason?.let { " (" + it + ")" } ?: ""))
        }
        return head + " — " + bits.joinToString(" · ")
    }

    /** One line per durable step + the gate verdict (drill-down). */
    fun stepLines(): List<String> {
        val lines = ArrayList<String>()
        for (step in steps) {
            val detail = step.detail?.takeIf { it.isNotEmpty() }?.let { " — " + it } ?: ""
            lines.add(
                "[" + step.status + "] " + step.step +
                    (if (step.present) "" else " (no durable status row)") + detail
            )
        }
        lines.add("gate: " + (gateStatus ?: "none") + (gateReason?.let { " — " + it } ?: ""))
        return lines
    }
}

/** Durable spend of the session task (tokens + microUSD, remaining computed). */
data class SpendSummary(
    val spentTokens: Long?,
    val maxTokens: Long?,
    /** Exact micro-USD (BigInteger; never a rounded float). */
    val spentCostMicro: BigInteger?,
    val maxCostMicro: BigInteger?,
    val openReservedMicro: BigInteger,
    val remainingTokens: Long?,
    val remainingCostMicro: BigInteger?,
    val durable: Boolean
)

/** One tournament candidate (verification/review verdicts + measured axes). */
data class TournamentCandidateView(
    val childId: String,
    val state: String,
    val presence: PixelPresence,
    val worktree: String,
    val baseRevision: String,
    val verification: Long?,
    val verificationPass: Boolean?,
    val reviewRank: String?,
    val reviewer: String?,
    val costMicro: BigInteger,
    val wallMs: Long,
    val winner: Boolean
)

/** The tournament view (candidates, winner state). */
data class TournamentView(
    val id: String,
    val runFamily: String,
    val goal: String,
    val criteria: List<NativeTournamentCriterion>,
    val candidates: List<TournamentCandidateView>,
    val winner: String?,
    val state: String
) {
    val decided: Boolean
        get() = winner != null

    /** Buttons are exposed while the engine can still act on the tournament. */
    val open: Boolean
        get() = state == "open" || state == "deciding"

    /** Decide is enabled once every candidate has settled (engine rule). */
    val canDecide: Boolean
        get() = open && candidates.isNotEmpty() && candidates.none { it.state == "running" }
}

/** The tournament the panel auto-loads on session open: the NEWEST summary. */
fun latestTournamentId(summaries: List<NativeTournamentSummary>): String? =
    summaries.lastOrNull()?.id

/** Everything the rich tree panel renders, built purely from native DTOs. */
data class TaskTreeModel(
    val goal: String,
    val state: String,
    val phase: String,
    val acceptanceCriteria: List<String>,
    /** Per-criterion proof: why Faktor believes each criterion holds. */
    val criteriaProof: List<CriterionProofRow> = emptyList(),
    val steps: List<StepNode>,
    val children: List<ChildNode>,
    val blockers: List<BlockerRow>,
    val taskBlockers: List<String>,
    val verification: VerificationSummary,
    val evidence: List<EvidenceRef>,
    val spend: SpendSummary?,
    val tournament: TournamentView?,
    /** The Task-mode completion contract + durable step statuses. */
    val completion: CompletionView? = null,
    /** The top-level VERIFIED view (strict proof summary; null = not fetched). */
    val proof: ProofView? = null
)

/** Pure builder over the native DTOs. */
object TaskTree {

    fun build(
        projection: NativeProjection? = null,
        task: NativeTaskView? = null,
        agents: List<NativeAgent> = emptyList(),
        graph: NativeOrchestratorGraph? = null,
        catalog: List<NativeModelInfo> = emptyList(),
        verification: NativeVerificationView? = null,
        taskVerification: NativeTaskVerification? = null,
        usage: NativeSessionUsage? = null,
        childUsage: Map<String, NativeSessionUsage> = emptyMap(),
        tournament: NativeTournament? = null,
        submittedCompletion: NativeCompletionContract? = null,
        runState: String? = null,
        proof: NativeTaskProof? = null,
        proofUnavailable: String? = null
    ): TaskTreeModel {
        val children = agents
            .filter { it.kind == "child" }
            // Background children are tucked after the foreground ones; the
            // sort is stable so the daemon's listing order survives inside
            // each group.
            .sortedBy { if (it.presentation == "background") 1 else 0 }
            .map { child ->
                childNode(child, catalog, childUsage[child.sessionId.toString()])
            }
        val goal = task?.goal?.takeIf { it.isNotEmpty() }
            ?: graph?.goal?.takeIf { it.isNotEmpty() }
            ?: children.firstOrNull()?.goal?.takeIf { it.isNotEmpty() }
            ?: ""
        val state = task?.state?.takeIf { it.isNotEmpty() }
            ?: projection?.machine
            ?: "unknown"
        val phase = task?.phase?.takeIf { it.isNotEmpty() }
            ?: projection?.label?.takeIf { it.isNotEmpty() }
            ?: state
        val criteria = acceptanceCriteria(task, taskVerification)
        return TaskTreeModel(
            goal = goal,
            state = state,
            phase = phase,
            acceptanceCriteria = criteria,
            criteriaProof = criteriaProof(task, taskVerification),
            steps = steps(task, graph, children),
            children = children,
            blockers = children.mapNotNull { blockerRow(it) },
            taskBlockers = task?.blockers ?: emptyList(),
            verification = verification(verification, taskVerification, task),
            evidence = evidence(task, taskVerification),
            spend = spend(usage, task),
            tournament = tournament?.let { tournamentView(it) },
            completion = completion(task, submittedCompletion, runState),
            proof = proof(proof, proofUnavailable)
        )
    }

    // --------------------------------------------------------------- proof

    /**
     * Project the strict proof DTO into the rendered view. A refusal
     * (`proofUnavailable`) wins over any cached DTO: the previous proof is
     * never re-rendered as VERIFIED for a read that can no longer be
     * confirmed.
     */
    private fun proof(
        proof: NativeTaskProof?,
        proofUnavailable: String?
    ): ProofView? {
        if (proofUnavailable != null) {
            return ProofView(
                state = "unavailable",
                reason = proofUnavailable,
                criteriaPassed = 0,
                criteriaTotal = 0,
                checksPassed = 0,
                checksTotal = 0,
                requiredPassed = 0,
                requiredTotal = 0,
                reviewStatus = null,
                reviewer = null,
                independentVerdict = "none",
                verifiedSnapshot = null,
                landedSnapshot = null,
                landedEqualsVerified = null,
                commitOid = null,
                remoteHeadOid = null,
                pullRequestId = null,
                costStatus = "unavailable",
                spentCostMicro = null,
                maxCostMicro = null,
                costReason = "proof read failed",
                gateStatus = null,
                gateReason = null,
                steps = emptyList(),
                unavailable = listOf(
                    ProofUnavailableView("proof", "unavailable", proofUnavailable)
                )
            )
        }
        if (proof == null) {
            return null
        }
        return ProofView(
            state = proof.proofState,
            reason = proof.proofStateReason,
            criteriaPassed = proof.criteriaPassed,
            criteriaTotal = proof.criteriaTotal,
            checksPassed = proof.checksPassed,
            checksTotal = proof.checksTotal,
            requiredPassed = proof.requiredPassed,
            requiredTotal = proof.requiredTotal,
            reviewStatus = proof.reviewStatus,
            reviewer = proof.reviewer,
            independentVerdict = proof.independentVerdict,
            verifiedSnapshot = proof.verifiedSnapshot,
            landedSnapshot = proof.landedSnapshot,
            landedEqualsVerified = proof.landedEqualsVerified,
            commitOid = proof.commitOid,
            remoteHeadOid = proof.remoteHeadOid,
            pullRequestId = proof.pullRequestId,
            costStatus = proof.costStatus,
            spentCostMicro = proof.spentCostMicro,
            maxCostMicro = proof.maxCostMicro,
            costReason = proof.costReason,
            gateStatus = proof.gateStatus,
            gateReason = proof.gateReason,
            steps = proof.steps.map {
                ProofStepView(
                    step = it.step,
                    status = it.status,
                    present = it.present,
                    detail = it.detail,
                    snapshot = it.snapshot,
                    atMs = it.atMs
                )
            },
            unavailable = proof.unavailable.map {
                ProofUnavailableView(it.component, it.kind, it.reason)
            }
        )
    }

    // ---------------------------------------------------------- completion

    /**
     * The Task-mode completion contract. The daemon-served rows win; without
     * a served read the block is derived from the DURABLE run state and the
     * submitted contract (pending / gate-certified / truthfully unknown).
     * A missing read is never presented as success.
     */
    private fun completion(
        task: NativeTaskView?,
        submitted: NativeCompletionContract?,
        runState: String?
    ): CompletionView? {
        val served = task?.completion
        if (served != null) {
            return CompletionView(
                includeCommit = served.contract.includeCommit,
                includePush = served.contract.includePush,
                includePr = served.contract.includePr,
                steps = served.steps.map {
                    CompletionStepView(it.step, it.status, it.detail.takeIf { detail -> detail.isNotEmpty() })
                },
                source = "daemon",
                reason = null
            )
        }
        val contract = submitted?.takeIf { !it.isDefault } ?: return null
        // Case-insensitive state match without String.lowercase()/toLowerCase():
        // both spellings are unusable across the supported kotlinc range
        // (1.3 has no lowercase(); >= 1.5 errors on toLowerCase()).
        val state = runState ?: task?.state ?: ""
        val requested = contract.requestedSteps()
        return when {
            state.equals("done", ignoreCase = true) -> CompletionView(
                includeCommit = contract.includeCommit,
                includePush = contract.includePush,
                includePr = contract.includePr,
                steps = requested.map {
                    CompletionStepView(
                        it,
                        "succeeded",
                        "certified by the durable completion gate (all requested steps succeeded)"
                    )
                },
                source = "derived",
                reason = null
            )
            state.equals("failed", ignoreCase = true) ||
                state.equals("cancelled", ignoreCase = true) -> CompletionView(
                includeCommit = contract.includeCommit,
                includePush = contract.includePush,
                includePr = contract.includePr,
                steps = requested.map {
                    CompletionStepView(it, "unknown", "the run ended before certification")
                },
                source = "unavailable",
                reason = "the serving daemon exposes no per-step completion read; exact statuses are not served"
            )
            else -> CompletionView(
                includeCommit = contract.includeCommit,
                includePush = contract.includePush,
                includePr = contract.includePr,
                steps = requested.map {
                    CompletionStepView(
                        it,
                        "pending",
                        "awaiting deterministic verification and the durable completion gate"
                    )
                },
                source = "derived",
                reason = null
            )
        }
    }

    // ------------------------------------------------------------ children

    private fun childNode(
        agent: NativeAgent,
        catalog: List<NativeModelInfo>,
        usage: NativeSessionUsage?
    ): ChildNode {
        // The child's provider rides the wire from its own durable session
        // row; the (provider, model) pair is the ONLY safe catalog join key
        // because two providers may expose the same model id with different
        // capability sets. A provider-less entry never guesses by model.
        val provider = agent.provider
        val info = if (agent.model == null || provider == null) {
            null
        } else {
            catalog.firstOrNull { it.provider == provider && it.model == agent.model }
        }
        var spentTokens: Long? = null
        var spentCostMicro: BigInteger? = null
        var maxCostMicro: BigInteger? = null
        var openReserved: BigInteger = BigInteger.ZERO
        if (usage != null) {
            for (taskUsage in usage.tasks) {
                val budget = taskUsage.budget
                budget.spentTokens?.let { spentTokens = (spentTokens ?: 0L) + it }
                // Exact BigInteger aggregation: a session total may exceed
                // 2^53-1 micro-USD and must never pass through a float.
                spentCostMicro = (spentCostMicro ?: BigInteger.ZERO) + budget.spentCostMicro
                budget.maxCostMicro?.let { maxCostMicro = (maxCostMicro ?: BigInteger.ZERO) + it }
                openReserved += budget.openReservedMicro
            }
            if (usage.tokens > 0 && spentTokens == null) spentTokens = usage.tokens
        }
        val maxTokens = agent.budget
        // kotlinc 1.3 (CI fallback toolchain) cannot smart-cast captured
        // vars inside closures: bind the current values first.
        val spentTokensNow = spentTokens
        val spentCostNow = spentCostMicro
        val remainingTokens = if (maxTokens == null || spentTokensNow == null) {
            null
        } else {
            (maxTokens - spentTokensNow).coerceAtLeast(0L)
        }
        val maxCostNow = maxCostMicro
        val remainingCost = if (maxCostNow == null || spentCostNow == null) {
            null
        } else {
            (maxCostNow - spentCostNow - openReserved).max(BigInteger.ZERO)
        }
        return ChildNode(
            childId = agent.agentId,
            runId = agent.runId,
            sessionId = agent.sessionId,
            itemId = agent.itemId,
            itemKind = agent.itemKind,
            goal = agent.goal,
            state = agent.state,
            presence = PixelAgents.presence(agent.agentId, agent.state),
            blocker = agent.blocker,
            model = agent.model,
            provider = provider,
            reasoning = info?.reasoning,
            tools = info?.tools,
            ownership = agent.ownership,
            worktreeId = agent.worktreeId,
            budgetMaxTokens = maxTokens,
            spentTokens = spentTokens,
            spentCostMicro = spentCostMicro,
            maxCostMicro = maxCostMicro,
            remainingTokens = remainingTokens,
            remainingCostMicro = remainingCost,
            progress = agent.progress,
            result = agent.result,
            presentation = agent.presentation
        )
    }

    private fun blockerRow(child: ChildNode): BlockerRow? {
        val blocker = child.blocker ?: return null
        val actions = when (blocker.kind) {
            "permission" -> listOf(
                BlockerAction.RESUME,
                BlockerAction.PERMISSION_ALLOW,
                BlockerAction.PERMISSION_DENY,
                BlockerAction.RETRY
            )
            "dependency" -> listOf(BlockerAction.RESUME, BlockerAction.RETRY)
            "budget" -> listOf(BlockerAction.RESUME, BlockerAction.RETRY)
            else -> listOf(BlockerAction.RESUME, BlockerAction.RETRY)
        }
        return BlockerRow(
            childId = child.childId,
            kind = blocker.kind,
            reason = blocker.reason,
            dependency = blocker.dependency,
            resolution = blocker.resolution,
            lastProgressMs = blocker.lastProgressMs,
            presence = child.presence,
            actions = actions
        )
    }

    // --------------------------------------------------------------- steps

    private fun steps(
        task: NativeTaskView?,
        graph: NativeOrchestratorGraph?,
        children: List<ChildNode>
    ): List<StepNode> {
        val plan = task?.plan ?: emptyList()
        if (plan.isNotEmpty()) {
            return plan.map { step ->
                StepNode(
                    id = step.id,
                    summary = step.summary,
                    state = step.state,
                    dependsOn = step.dependsOn,
                    childIds = children.filter { it.itemId == step.id }.map { it.childId }
                )
            }
        }
        if (graph != null && graph.workItems.isNotEmpty()) {
            return graph.workItems.mapIndexed { index, item ->
                StepNode(
                    id = item.itemId,
                    summary = item.kind,
                    state = item.state,
                    dependsOn = emptyList(),
                    childIds = graph.children
                        .filter { it.planStepIndex == index }
                        .map { it.childId }
                )
            }
        }
        if (task != null) {
            return task.milestones.completed.map { text ->
                StepNode(text, text, "done", emptyList(), children.filter { it.itemId == text }.map { it.childId })
            } + task.milestones.open.map { text ->
                StepNode(text, text, "open", emptyList(), children.filter { it.itemId == text }.map { it.childId })
            }
        }
        return emptyList()
    }

    // ----------------------------------------------------- criteria/evidence

    private fun acceptanceCriteria(
        task: NativeTaskView?,
        taskVerification: NativeTaskVerification?
    ): List<String> {
        val explicit = (task?.acceptanceCriteria ?: emptyList()).filter { it.trim().isNotEmpty() }
        if (explicit.isNotEmpty()) return explicit
        val fromRecords = ArrayList<String>()
        for (record in taskVerification?.records ?: emptyList()) {
            for (criterion in record.criteria) {
                if (!fromRecords.contains(criterion.criterionKey)) {
                    fromRecords.add(criterion.criterionKey)
                }
                if (fromRecords.size >= MAX_TREE_EVIDENCE) return fromRecords
            }
        }
        return fromRecords
    }

    // --------------------------------------------------- criterion proof

    private const val REQUIREMENT_UNAVAILABLE =
        "unavailable (this row carries no criterion requirement member)"
    private const val ORIGIN_UNAVAILABLE =
        "unavailable (this row carries no criterion origin member)"
    private const val SNAPSHOT_UNAVAILABLE =
        "unavailable (this record carries no candidate/verified/based-on/landed snapshot or source count)"
    private const val TIMESTAMP_UNAVAILABLE =
        "unavailable (this record carries no startedMs/completedMs)"

    private val EVIDENCE_ID_PATTERN = Regex("^evidence:[0-9]+$")

    private data class DerivedBinding(
        val kind: String,
        val reference: String?,
        val detail: String?
    )

    /**
     * The per-criterion proof rows: explicit task criteria first (a missing
     * verdict is an honest unavailable), then the record's extra certified
     * criteria. Bounded like every other list.
     */
    private fun criteriaProof(
        task: NativeTaskView?,
        taskVerification: NativeTaskVerification?
    ): List<CriterionProofRow> {
        val records = taskVerification?.records ?: emptyList()
        val explicit = (task?.acceptanceCriteria ?: emptyList())
            .map { it.trim() }
            .filter { it.isNotEmpty() }
        val rows = ArrayList<CriterionProofRow>()
        val explicitSet = explicit.toSet()
        for (key in explicit) {
            var matchedRecord: NativeVerificationRecord? = null
            var matched: NativeCriterionVerdict? = null
            for (record in records) {
                val criterion = record.criteria.firstOrNull { it.criterionKey == key }
                if (criterion != null) {
                    matchedRecord = record
                    matched = criterion
                    break
                }
            }
            rows.add(criterionProofRow(key, matched, matchedRecord))
            if (rows.size >= MAX_CRITERIA_PROOF_ROWS) return rows
        }
        for (record in records) {
            for (criterion in record.criteria) {
                if (criterion.criterionKey.isNotEmpty() && explicitSet.contains(criterion.criterionKey)) {
                    continue
                }
                rows.add(criterionProofRow(criterion.criterionKey, criterion, record))
                if (rows.size >= MAX_CRITERIA_PROOF_ROWS) return rows
            }
        }
        return rows
    }

    /**
     * One proof row. The wire-served proof annotations win: the typed binding
     * (kind + members), the three-way verdict and the record-level
     * snapshots/timestamps render directly. Only when the serving daemon
     * predates a member does the row fall back — a binding MAY still be
     * DERIVED from unambiguous typed evidence refs (check:/file:/work-item:/
     * evidence:<n>), and every genuinely missing member stays explicit
     * unavailable text. Nothing is fabricated and an unavailable row can
     * never render as a pass.
     */
    private fun criterionProofRow(
        criterionKey: String,
        criterion: NativeCriterionVerdict?,
        record: NativeVerificationRecord?
    ): CriterionProofRow {
        val refs = evidenceRefs(criterion?.evidence)
        val unavailable = ArrayList<String>()

        val requirement = criterion?.requirement?.takeIf { it.isNotEmpty() }
        if (requirement == null) unavailable.add("requirement")
        val origin = criterion?.origin?.takeIf { it.isNotEmpty() }
        if (origin == null) unavailable.add("origin")

        val served = criterion?.binding
        val bindingKind: String
        val bindingSource: String
        val bindingReference: String?
        val bindingDetail: String?
        if (served != null) {
            bindingKind = served.kind
            bindingSource = "daemon"
            bindingReference = bindingReferenceOf(served)
            bindingDetail = bindingDetailOf(served)
        } else {
            val derived = deriveBinding(refs.map { it.label })
            if (derived != null) {
                bindingKind = derived.kind
                bindingSource = "derived"
                bindingReference = derived.reference
                bindingDetail = derived.detail
            } else {
                bindingKind = "unavailable"
                bindingSource = "unavailable"
                bindingReference = null
                bindingDetail = if (criterion == null) {
                    "no durable verification record covers this criterion"
                } else if (refs.isEmpty()) {
                    "this row carries no criterion binding and no typed evidence ref identifies one"
                } else {
                    "this row carries no criterion binding and the evidence refs do not identify one kind"
                }
                unavailable.add("binding")
            }
        }

        var verdict = "unavailable"
        var verdictReason: String? = null
        val servedVerdict = normalizeVerdict(criterion?.verdict)
        if (servedVerdict != null) {
            verdict = servedVerdict
        } else if (criterion == null) {
            verdictReason = "no durable verification record was served for this criterion"
            unavailable.add("verdict")
        } else if (criterion.passed) {
            verdict = "pass"
        } else {
            verdict = "fail"
            verdictReason =
                "recorded as not passed; this row carries no three-way verdict, so failed and unavailable cannot be distinguished here"
        }

        val snapshot = snapshotText(record)
        if (snapshot == null) unavailable.add("snapshots")
        val timestamp = timestampText(record)
        if (timestamp == null) unavailable.add("verification timestamp")

        return CriterionProofRow(
            criterionKey = criterionKey,
            requirement = requirement ?: REQUIREMENT_UNAVAILABLE,
            origin = origin ?: ORIGIN_UNAVAILABLE,
            bindingKind = bindingKind,
            bindingSource = bindingSource,
            bindingReference = bindingReference,
            bindingDetail = bindingDetail,
            verdict = verdict,
            verdictReason = verdictReason,
            evidenceRefs = refs,
            snapshot = snapshot ?: SNAPSHOT_UNAVAILABLE,
            verificationTimestamp = timestamp ?: TIMESTAMP_UNAVAILABLE,
            recordId = record?.recordId,
            recordStatus = record?.status,
            unavailable = unavailable
        )
    }

    /** Normalize the daemon's three-way verdict spellings (never a guess). */
    private fun normalizeVerdict(raw: String?): String? = when (raw) {
        "pass", "passed" -> "pass"
        "fail", "failed" -> "fail"
        "unavailable" -> "unavailable"
        else -> null
    }

    /** The exact typed reference of a served binding (mirrors the cockpit). */
    private fun bindingReferenceOf(binding: NativeCriterionBinding): String? =
        when (binding.kind) {
            "required_check" -> binding.checkId?.let { check ->
                "check:" + check + (binding.commandDigest?.let { ":" + it } ?: "")
            }
            "integration_coverage" ->
                if (binding.requiredWorkItems.isEmpty()) {
                    null
                } else {
                    binding.requiredWorkItems.joinToString(", ") { "work-item:" + it }
                }
            "file_state" -> binding.path
            "evidence" -> binding.evidenceId?.let { "evidence:" + it }
            "independent_review" -> binding.reviewerId
            else -> null
        }

    /** The human detail of a served binding (never synthesizes a member). */
    private fun bindingDetailOf(binding: NativeCriterionBinding): String? =
        when (binding.kind) {
            "required_check" -> binding.checkId?.let { check ->
                "check " + check +
                    (binding.commandDigest?.let { " · command digest " + it } ?: "")
            }
            "integration_coverage" -> if (binding.requiredWorkItems.isEmpty()) {
                null
            } else {
                binding.requiredWorkItems.size.toString() + " required work item(s)"
            }
            "file_state" -> binding.expectedDigest?.let { "expected digest " + it }
                ?: binding.path?.let { "path " + it }
            "evidence" -> binding.evidenceDigest?.let { "evidence digest " + it }
                ?: binding.evidenceId?.let { "evidence " + it }
            "independent_review" -> binding.reviewerId?.let { "reviewer " + it }
            "aggregate_goal" -> "all subordinate criteria + the independent final review"
            "unavailable" -> binding.reason ?: "the served binding is explicitly unavailable"
            else -> binding.kind
        }

    /** The served proof snapshots; null only when the record carries none. */
    private fun snapshotText(record: NativeVerificationRecord?): String? {
        if (record == null) return null
        val proof = record.candidateProof
        val bits = ArrayList<String>()
        proof?.candidateSnapshot?.let { bits.add("cand " + digestLabel(it)) }
        record.verifiedSnapshot?.let { bits.add("ver " + digestLabel(it)) }
        record.basedOnSnapshot?.let { bits.add("base " + digestLabel(it)) }
        record.landedSnapshot?.let { bits.add("land " + digestLabel(it)) }
        record.sourceCount?.let { bits.add("src " + it) }
        proof?.publishedCommit?.let { bits.add("commit " + digestLabel(it)) }
        proof?.remotePrHead?.let { bits.add("head " + digestLabel(it)) }
        record.reviewer?.takeIf { it.isNotEmpty() }?.let { bits.add("review " + it) }
        return if (bits.isEmpty()) null else bits.joinToString(" · ")
    }

    /** The served verification timestamps; null only when none is carried. */
    private fun timestampText(record: NativeVerificationRecord?): String? {
        if (record == null) return null
        val bits = ArrayList<String>()
        record.startedMs?.let { bits.add("started " + it + "ms") }
        record.completedMs?.let { bits.add("completed " + it + "ms") }
        return if (bits.isEmpty()) null else bits.joinToString(" · ")
    }

    /** Bounded digest label so the proof facts survive the panel clamp. */
    private fun digestLabel(digest: String): String = shortDigest(digest)

    /** Split the record's evidence string into its typed refs (bounded). */
    private fun evidenceRefs(raw: String?): List<EvidenceRef> {
        if (raw == null || raw.trim().isEmpty()) return emptyList()
        val out = ArrayList<EvidenceRef>()
        val seen = HashSet<String>()
        for (segment in raw.split(";")) {
            val trimmed = segment.trim()
            if (trimmed.isEmpty() || out.size >= MAX_TREE_EVIDENCE) continue
            val ref = EvidenceRefs.parse(trimmed)
            val dedupe = (ref.id?.toString() ?: "text") + ":" + ref.label
            if (seen.add(dedupe)) out.add(ref)
        }
        return out
    }

    /** Read the binding kind from the criterion's TYPED evidence refs. */
    private fun deriveBinding(rawRefs: List<String>): DerivedBinding? {
        val refs = rawRefs.map { it.trim() }.filter { it.isNotEmpty() }
        if (refs.isEmpty()) return null
        if (refs.size == 1 && refs[0].startsWith("check:")) {
            val rest = refs[0].substring("check:".length)
            val cut = rest.lastIndexOf(':')
            return if (cut > 0) {
                DerivedBinding(
                    "required_check",
                    refs[0],
                    "check " + rest.substring(0, cut) + " · command digest " + rest.substring(cut + 1)
                )
            } else {
                DerivedBinding("required_check", refs[0], "check " + rest)
            }
        }
        if (refs.size == 1 && refs[0].startsWith("file:")) {
            return DerivedBinding(
                "file_state",
                refs[0],
                "path " + refs[0].substring("file:".length)
            )
        }
        if (refs.all { it.startsWith("work-item:") }) {
            return DerivedBinding(
                "integration_coverage",
                refs.joinToString(", "),
                refs.size.toString() + " work-item contribution(s)"
            )
        }
        if (refs.size == 1 && EVIDENCE_ID_PATTERN.matches(refs[0])) {
            return DerivedBinding(
                "evidence",
                refs[0],
                "evidence " + refs[0].substring("evidence:".length)
            )
        }
        return null
    }

    private fun evidence(
        task: NativeTaskView?,
        taskVerification: NativeTaskVerification?
    ): List<EvidenceRef> {
        val out = ArrayList<EvidenceRef>()
        val seen = HashSet<String>()
        fun push(raw: String?) {
            if (raw == null || raw.trim().isEmpty()) return
            if (out.size >= MAX_TREE_EVIDENCE) return
            val ref = EvidenceRefs.parse(raw)
            val dedupe = (ref.id?.toString() ?: "text") + ":" + ref.label
            if (seen.add(dedupe)) out.add(ref)
        }
        for (record in taskVerification?.records ?: emptyList()) {
            for (criterion in record.criteria) push(criterion.evidence)
            for (summary in record.checkSummaries) push(summary)
        }
        for (ref in task?.evidenceRefs ?: emptyList()) push(ref)
        return out
    }

    // --------------------------------------------------------- verification

    private fun verification(
        verification: NativeVerificationView?,
        taskVerification: NativeTaskVerification?,
        task: NativeTaskView?
    ): VerificationSummary {
        var passed = 0
        var total = 0
        var recordStatus: String? = null
        var topRecord: NativeVerificationRecord? = null
        for (record in taskVerification?.records ?: emptyList()) {
            recordStatus = record.status
            topRecord = record
            passed += record.criteriaPassed
            total += record.criteriaTotal
        }
        val failed = verification?.failedChecks?.size ?: 0
        val owed = verification?.owed?.size ?: 0
        val status = recordStatus ?: when {
            total > 0 && passed == total -> "passed"
            failed > 0 -> "failed"
            owed > 0 -> "pending"
            else -> task?.state ?: "unknown"
        }
        val verified = topRecord != null &&
            topRecord.status.equals("passed", ignoreCase = true) &&
            total > 0 &&
            passed == total &&
            failed == 0
        return VerificationSummary(
            status = status,
            criteriaPassed = passed,
            criteriaTotal = total,
            failedChecks = failed,
            owed = owed,
            recordStatus = recordStatus,
            verified = verified,
            reviewer = topRecord?.reviewer?.takeIf { it.isNotEmpty() },
            tree = topRecord?.verifiedSnapshot?.takeIf { it.isNotEmpty() },
            publishedCommit = topRecord?.candidateProof?.publishedCommit?.takeIf { it.isNotEmpty() },
            remotePrHead = topRecord?.candidateProof?.remotePrHead?.takeIf { it.isNotEmpty() }
        )
    }

    // ---------------------------------------------------------------- spend

    private fun spend(usage: NativeSessionUsage?, task: NativeTaskView?): SpendSummary? {
        if (usage != null) {
            var spentTokens: Long? = null
            var maxTokens: Long? = null
            var spentCost: BigInteger? = null
            var maxCost: BigInteger? = null
            var open = BigInteger.ZERO
            for (taskUsage in usage.tasks) {
                val budget = taskUsage.budget
                budget.spentTokens?.let { spentTokens = (spentTokens ?: 0L) + it }
                budget.maxTokens?.let { maxTokens = (maxTokens ?: 0L) + it }
                // Exact BigInteger aggregation: never a float on money.
                spentCost = (spentCost ?: BigInteger.ZERO) + budget.spentCostMicro
                budget.maxCostMicro?.let { maxCost = (maxCost ?: BigInteger.ZERO) + it }
                open += budget.openReservedMicro
            }
            if (spentTokens == null && usage.tokens > 0) spentTokens = usage.tokens
            return SpendSummary(
                spentTokens = spentTokens,
                maxTokens = maxTokens,
                spentCostMicro = spentCost,
                maxCostMicro = maxCost,
                openReservedMicro = open,
                remainingTokens = remainingTokens(maxTokens, spentTokens),
                remainingCostMicro = remainingCost(maxCost, spentCost, open),
                durable = true
            )
        }
        val budget = task?.budget ?: return null
        return SpendSummary(
            spentTokens = budget.spentTokens,
            maxTokens = budget.maxTokens,
            spentCostMicro = budget.spentCostMicro,
            maxCostMicro = budget.maxCostMicro,
            openReservedMicro = budget.openReservedMicro,
            remainingTokens = remainingTokens(budget.maxTokens, budget.spentTokens),
            remainingCostMicro = remainingCost(
                budget.maxCostMicro,
                budget.spentCostMicro,
                budget.openReservedMicro
            ),
            durable = true
        )
    }

    private fun remainingTokens(max: Long?, spent: Long?): Long? {
        if (max == null || spent == null) return null
        return (max - spent).coerceAtLeast(0L)
    }

    private fun remainingCost(max: BigInteger?, spent: BigInteger?, open: BigInteger): BigInteger? {
        if (max == null || spent == null) return null
        return (max - spent - open).max(BigInteger.ZERO)
    }

    // ----------------------------------------------------------- tournament

    fun tournamentView(tournament: NativeTournament): TournamentView =
        TournamentView(
            id = tournament.id,
            runFamily = tournament.runFamily,
            goal = tournament.goal,
            criteria = tournament.criteria,
            candidates = tournament.candidates.map { candidate ->
                candidateView(candidate, tournament.winner)
            },
            winner = tournament.winner,
            state = tournament.state
        )

    private fun candidateView(
        candidate: NativeTournamentCandidate,
        winner: String?
    ): TournamentCandidateView = TournamentCandidateView(
        childId = candidate.childId,
        state = candidate.state,
        presence = PixelAgents.presence(candidate.childId, candidate.state),
        worktree = candidate.worktree,
        baseRevision = candidate.baseRevision,
        verification = candidate.verification,
        verificationPass = candidate.verificationPass,
        reviewRank = candidate.reviewRank,
        reviewer = candidate.reviewer,
        costMicro = candidate.costMicro,
        wallMs = candidate.wallMs,
        winner = winner != null && winner == candidate.childId
    )
}
