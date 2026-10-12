// I03 — candidate changes and the native immutable diff review.
//
// This file is the PURE core of the Changes surface: it derives every row
// fact from SERVED data only (the session projection's `filesChanged`, the
// current run's `NativeTaskView.changedFiles`, the verification records'
// `changedFiles` and their criteria bindings), resolves the immutable review
// basis (candidate proof `sourceDiffEvidence`, then a change's attached
// evidence refs), and drives a host seam that can ONLY receive artifact
// BYTES — it has no parameter, method or field that names a live workspace
// file, so no code path here can ever open one.
//
// The platform editor APIs (FileEditorManager / LightVirtualFile /
// DiffManager) live exclusively in ChangeReviewAdapter.kt on the other side
// of [ChangeReviewHost]; the kotlinc smoke drives the controller with a fake
// host and a fake evidence retriever.
package dev.faktor.frontend

import dev.faktor.shared.NativeApiException
import dev.faktor.shared.NativeCandidateProof
import dev.faktor.shared.NativeEvidenceRetrieval
import dev.faktor.shared.NativeProjection
import dev.faktor.shared.NativeProtocolException
import dev.faktor.shared.NativeTaskVerification
import dev.faktor.shared.NativeTaskView
import dev.faktor.shared.NativeVerificationRecord
import java.nio.charset.StandardCharsets

/** Max change rows one Changes surface renders (bounded like every list). */
const val MAX_CHANGE_ROWS = 200

/** Max attached refs rendered per row (bounded). */
const val MAX_CHANGE_ROW_REFS = 16

/** Exact empty-state copy (I03). */
const val CHANGES_EMPTY_COPY = "No candidate changes are served for this run yet."

/** Exact diff-unavailable label and copy (I03). */
const val DIFF_UNAVAILABLE_LABEL = "Diff unavailable"
const val DIFF_UNAVAILABLE_COPY =
    "The runtime does not serve an immutable candidate content read for this change."

/** The first line prepended when the served artifact was truncated by policy. */
const val TRUNCATED_BY_POLICY_HEADER =
    "[truncated by policy: the runtime served a bounded prefix of this artifact]"

/** Read-failure headline for the Changes surface. */
const val CHANGES_UNAVAILABLE_LABEL = "Changes unavailable"

/**
 * One change row. Facts come only from served relationships:
 *  - [path] from the projection / task view / verification record changedFiles;
 *  - [verificationRecordIds] from verification records whose `changedFiles`
 *    contain the path;
 *  - [criterionKeys]/[evidenceRefs]/[expectedDigest] from criteria whose
 *    served binding is a `file_state` on this path (or whose typed `file:`
 *    evidence ref names it exactly);
 *  - [changeKind] stays null unless a daemon serves a kind (none does today:
 *    nothing is fabricated).
 */
data class ChangeRow(
    val path: String,
    val changeKind: String?,
    val verificationRecordIds: List<String>,
    val criterionKeys: List<String>,
    val evidenceRefs: List<EvidenceRef>,
    val expectedDigest: String?
)

/**
 * The served immutable basis of one run's review: the candidate identity the
 * scope belongs to (human display; raw hashes only as advanced detail) and
 * the source-diff evidence id, when the daemon serves one.
 */
data class ChangeBasis(
    /** Human candidate display, or null when nothing identifies the candidate. */
    val candidateLabel: String?,
    /** Advanced detail lines (served raw hashes only). */
    val candidateDetail: List<String>,
    /** The parsed source-diff evidence id; null when absent or unparseable. */
    val sourceDiffEvidenceId: Long?,
    /** The raw served `sourceDiffEvidence` value (advanced detail). */
    val sourceDiffEvidenceRaw: String?,
    val runId: String?
)

/** The pure reviewed state of one Changes surface. */
data class ChangeReviewModel(
    val basis: ChangeBasis?,
    val rows: List<ChangeRow>,
    /** Read-failure reason: rows are cleared and the surface says unavailable. */
    val unavailable: String? = null
) {
    val isEmpty: Boolean
        get() = rows.isEmpty()

    val emptyCopy: String
        get() = CHANGES_EMPTY_COPY
}

/**
 * Build the change-review model from served payloads. `unavailable` is the
 * explicit read-failure reason (never stale rows behind it: the builder
 * returns an empty row set).
 */
fun changeReviewOf(
    projection: NativeProjection?,
    task: NativeTaskView?,
    taskVerification: NativeTaskVerification?,
    unavailable: String? = null
): ChangeReviewModel {
    if (unavailable != null) {
        return ChangeReviewModel(basis = null, rows = emptyList(), unavailable = unavailable)
    }
    val paths = LinkedHashSet<String>()
    for (path in task?.changedFiles.orEmpty()) {
        if (path.isNotBlank() && paths.size < MAX_CHANGE_ROWS) paths.add(path)
    }
    for (path in projection?.filesChanged.orEmpty()) {
        if (path.isNotBlank() && paths.size < MAX_CHANGE_ROWS) paths.add(path)
    }
    val records = taskVerification?.records ?: emptyList()
    val rows = ArrayList<ChangeRow>()
    for (path in paths) {
        rows.add(changeRowOf(path, records))
    }
    return ChangeReviewModel(basis = basisOf(records), rows = rows)
}

private fun changeRowOf(path: String, records: List<NativeVerificationRecord>): ChangeRow {
    val recordIds = ArrayList<String>()
    val criterionKeys = ArrayList<String>()
    val evidenceRefs = ArrayList<EvidenceRef>()
    val seenRefs = HashSet<String>()
    var expectedDigest: String? = null
    for (record in records) {
        if (record.changedFiles.contains(path) && !recordIds.contains(record.recordId)) {
            recordIds.add(record.recordId)
        }
        for (criterion in record.criteria) {
            val binding = criterion.binding
            val boundToPath = (binding != null && binding.kind == "file_state" && binding.path == path)
            val typedFileRef = criterion.evidence
                ?.split(";")
                ?.map { it.trim() }
                ?.any { it == "file:" + path }
                ?: false
            if (!boundToPath && !typedFileRef) continue
            if (criterion.criterionKey.isNotEmpty() && !criterionKeys.contains(criterion.criterionKey)) {
                criterionKeys.add(criterion.criterionKey)
            }
            if (binding != null && binding.kind == "file_state" && binding.path == path) {
                binding.expectedDigest?.takeIf { it.isNotEmpty() }?.let { digest ->
                    if (expectedDigest == null) expectedDigest = digest
                }
            }
            criterion.evidence?.split(";")?.forEach { raw ->
                if (evidenceRefs.size >= MAX_CHANGE_ROW_REFS) return@forEach
                val ref = EvidenceRefs.parse(raw)
                val dedupe = (ref.id?.toString() ?: "text") + ":" + ref.label
                if (seenRefs.add(dedupe)) evidenceRefs.add(ref)
            }
        }
    }
    return ChangeRow(
        path = path,
        changeKind = null,
        verificationRecordIds = recordIds,
        criterionKeys = criterionKeys,
        evidenceRefs = evidenceRefs,
        expectedDigest = expectedDigest
    )
}

/**
 * The served candidate proof of the current run: the record with the latest
 * completed timestamp when the daemon serves timestamps, else the last
 * record carrying one. Never invents a member.
 */
internal fun candidateProofRecord(
    records: List<NativeVerificationRecord>
): NativeVerificationRecord? {
    val withProof = records.filter { it.candidateProof != null }
    if (withProof.isEmpty()) return null
    val timed = withProof.filter { it.completedMs != null }
    if (timed.isNotEmpty()) return timed.maxByOrNull { it.completedMs ?: Long.MIN_VALUE }
    return withProof.last()
}

private fun basisOf(records: List<NativeVerificationRecord>): ChangeBasis? {
    val record = candidateProofRecord(records) ?: return null
    val proof = record.candidateProof ?: return null
    val label = proof.candidateSnapshot?.takeIf { it.isNotEmpty() }?.let { "candidate " + shortDigest(it) }
        ?: proof.runId?.takeIf { it.isNotEmpty() }?.let { "run $it" }
    val detail = ArrayList<String>()
    fun line(name: String, value: String?) {
        if (value != null && value.isNotEmpty()) detail.add("$name: $value")
    }
    line("run", proof.runId)
    line("base manifest", proof.baseManifestHash)
    line("candidate manifest", proof.candidateManifestHash)
    line("run base snapshot", proof.runBaseSnapshot)
    line("candidate snapshot", proof.candidateSnapshot)
    line("verified snapshot", record.verifiedSnapshot)
    line("based on snapshot", record.basedOnSnapshot)
    line("sources digest", proof.sourcesDigest)
    line("changed files digest", proof.changedFilesDigest)
    line("source diff evidence", proof.sourceDiffEvidence)
    line("risk report evidence", proof.riskReportEvidence)
    line("accounting snapshot", proof.accountingSnapshotDigest)
    line("published commit", proof.publishedCommit)
    line("remote PR head", proof.remotePrHead)
    val raw = proof.sourceDiffEvidence
    return ChangeBasis(
        candidateLabel = label,
        candidateDetail = detail,
        sourceDiffEvidenceId = raw?.let { EvidenceRefs.parse(it).id },
        sourceDiffEvidenceRaw = raw,
        runId = proof.runId
    )
}

// ------------------------------------------------------------- host seam

/**
 * One immutable artifact opened in the editor area. [text] is the exact
 * served payload (with the truncation header as its first line when the
 * runtime bounded it); the artifact is identified only by its evidence id
 * and immutable title — there is no workspace file identity anywhere.
 */
data class ImmutableArtifact(
    val title: String,
    val text: String,
    val truncatedByPolicy: Boolean,
    val evidenceId: Long
)

/**
 * The platform seam. It accepts immutable artifact BYTES only; the interface
 * deliberately has no method that takes a file path, so no implementation
 * can be handed a live workspace file through this seam.
 */
interface ChangeReviewHost {
    fun openArtifact(artifact: ImmutableArtifact)

    /** A true two-sided diff of two immutable artifacts for the same path. */
    fun openDiff(title: String, left: ImmutableArtifact, right: ImmutableArtifact)
}

/** The typed outcome of one review open attempt. */
sealed class ChangeOpenResult {
    object Opened : ChangeOpenResult()

    /** No immutable basis is served: the exact I03 in-panel state applies. */
    object Unavailable : ChangeOpenResult()

    /** A served value or the daemon refused: report the typed reason. */
    data class Refused(val reason: String) : ChangeOpenResult()
}

/** The resolved immutable basis of one open request. */
sealed class BasisResolution {
    data class Single(val evidenceId: Long, val title: String) : BasisResolution()

    data class Pair(val first: Long, val second: Long, val title: String) : BasisResolution()

    /** A served basis value that is not a valid evidence id (typed refusal). */
    data class Invalid(val reason: String) : BasisResolution()

    object Unavailable : BasisResolution()
}

/**
 * Resolve the immutable basis of the selected change in the exact I03 order:
 *
 *  a. the served candidate `sourceDiffEvidence` evidence id;
 *  b. else the selected path's attached evidence refs (a two-sided diff only
 *     when TWO immutable artifacts for the same path are served);
 *  c. else an explicit unavailable (the in-panel `Diff unavailable` state).
 *
 * A served-but-unparseable id is a typed refusal, never silently skipped.
 */
fun resolveChangeBasis(model: ChangeReviewModel, rowIndex: Int?): BasisResolution {
    val basis = model.basis
    val raw = basis?.sourceDiffEvidenceRaw
    if (raw != null) {
        val parsed = EvidenceRefs.parse(raw).id
            ?: return BasisResolution.Invalid(
                "the served source diff evidence \"${bound(raw, 120)}\" is not a valid evidence id"
            )
        val display = basis.candidateLabel ?: "candidate"
        return BasisResolution.Single(parsed, "$display — source diff — evidence #$parsed (immutable)")
    }
    val row = model.rows.getOrNull(rowIndex ?: -1)
    if (row != null) {
        val ids = row.evidenceRefs.mapNotNull { it.id }.distinct()
        if (ids.size >= 2) {
            return BasisResolution.Pair(
                ids[0],
                ids[1],
                "${row.path} — evidence #${ids[0]} \u2194 evidence #${ids[1]} (immutable)"
            )
        }
        if (ids.size == 1) {
            return BasisResolution.Single(ids[0], "${row.path} — evidence #${ids[0]} (immutable)")
        }
    }
    return BasisResolution.Unavailable
}

/**
 * Execute one immutable open through [host]. Retrieval is the caller's
 * closure (the production wiring uses `NativeClient.retrieveEvidence` with
 * the session-scoped selector `{"selector":"all"}`); a retrieval that
 * answers with a DIFFERENT evidence id than the requested one is a typed
 * refusal (cross-session/foreign evidence can never be opened as this
 * run's basis).
 */
class ChangeReviewController(
    private val host: ChangeReviewHost,
    private val retrieve: (Long) -> NativeEvidenceRetrieval
) {

    fun open(model: ChangeReviewModel, rowIndex: Int?): ChangeOpenResult =
        when (val resolution = resolveChangeBasis(model, rowIndex)) {
            is BasisResolution.Unavailable -> ChangeOpenResult.Unavailable
            is BasisResolution.Invalid -> ChangeOpenResult.Refused(resolution.reason)
            is BasisResolution.Single -> {
                val artifact = retrieveArtifact(resolution.evidenceId, resolution.title)
                    ?: return ChangeOpenResult.Refused(lastRefusal ?: "the evidence read was refused")
                host.openArtifact(artifact)
                ChangeOpenResult.Opened
            }
            is BasisResolution.Pair -> {
                val left = retrieveArtifact(resolution.first, resolution.title)
                    ?: return ChangeOpenResult.Refused(lastRefusal ?: "the evidence read was refused")
                val right = retrieveArtifact(resolution.second, resolution.title)
                    ?: return ChangeOpenResult.Refused(lastRefusal ?: "the evidence read was refused")
                host.openDiff(resolution.title, left, right)
                ChangeOpenResult.Opened
            }
        }

    /** The last typed refusal message (the panel renders it verbatim). */
    var lastRefusal: String? = null
        private set

    private fun retrieveArtifact(evidenceId: Long, title: String): ImmutableArtifact? {
        lastRefusal = null
        val retrieval = try {
            retrieve(evidenceId)
        } catch (e: NativeApiException) {
            lastRefusal = "evidence #$evidenceId refused (status ${e.status} ${e.code}: ${e.detail})"
            return null
        } catch (e: NativeProtocolException) {
            lastRefusal = "evidence #$evidenceId refused: ${e.detail}"
            return null
        } catch (e: Exception) {
            lastRefusal = "evidence #$evidenceId read failed: ${e.message ?: e.javaClass.simpleName}"
            return null
        }
        if (retrieval.id != evidenceId) {
            lastRefusal =
                "evidence #$evidenceId refused: the runtime answered with evidence #${retrieval.id} " +
                    "(a foreign artifact is never opened as this run's basis)"
            return null
        }
        val text = String(retrieval.bytes, StandardCharsets.UTF_8)
        val body = if (retrieval.truncatedByPolicy) {
            TRUNCATED_BY_POLICY_HEADER + "\n" + text
        } else {
            text
        }
        return ImmutableArtifact(
            title = title,
            text = body,
            truncatedByPolicy = retrieval.truncatedByPolicy,
            evidenceId = evidenceId
        )
    }
}

/**
 * Re-derive the conflict facts of a `Recheck current basis` (S15): the
 * comparison of the last captured served basis against a fresh read. There
 * is no rebase/apply control because no such capability exists; the facts
 * are informational text only.
 */
fun basisConflictFacts(previous: ChangeReviewModel?, current: ChangeReviewModel): List<String> {
    if (current.unavailable != null) {
        return listOf("the served basis could not be re-read: ${current.unavailable}")
    }
    if (previous == null || previous.unavailable != null) {
        val basis = current.basis
        val basisText = basis?.sourceDiffEvidenceId?.let { "source diff evidence #$it" }
            ?: "no served source diff evidence"
        return listOf(
            "no prior served basis was captured; the current basis is $basisText " +
                "with ${plural(current.rows.size, "change")}"
        )
    }
    val facts = ArrayList<String>()
    val before = previous.basis
    val after = current.basis
    if (before?.sourceDiffEvidenceRaw != after?.sourceDiffEvidenceRaw) {
        facts.add(
            "conflict: the served source diff basis changed (" +
                describeEvidence(before?.sourceDiffEvidenceRaw) + " -> " +
                describeEvidence(after?.sourceDiffEvidenceRaw) + ")"
        )
    }
    if (before?.candidateLabel != after?.candidateLabel) {
        facts.add(
            "conflict: the served candidate identity changed (" +
                describeValue(before?.candidateLabel) + " -> " + describeValue(after?.candidateLabel) + ")"
        )
    }
    val beforePaths = previous.rows.map { it.path }.toSet()
    val afterPaths = current.rows.map { it.path }.toSet()
    if (beforePaths != afterPaths) {
        facts.add(
            "conflict: the served changed-file set changed (" +
                plural(beforePaths.size, "path") + " -> " + plural(afterPaths.size, "path") + ")"
        )
    }
    if (facts.isEmpty()) {
        val basis = current.basis
        val basisText = basis?.sourceDiffEvidenceId?.let { "source diff evidence #$it" }
            ?: "no served source diff evidence"
        facts.add("the served basis is unchanged: $basisText, ${plural(current.rows.size, "change")}")
    }
    return facts
}

private fun describeEvidence(raw: String?): String =
    if (raw == null) "none" else "#" + (EvidenceRefs.parse(raw).id?.toString() ?: bound(raw, 60))

private fun describeValue(value: String?): String = value ?: "none"
