// Frontend smoke: the new model parsing (blocker, tournament,
// provider/reasoning, attachments) against canned native JSON, a real
// daemon where feasible (task-run with `files`, permissions reply route,
// unknown-tournament typed 404, best-effort graph), and construction of
// EVERY new UI section from a mock payload (no display needed: Swing
// components are created but never shown).
package dev.faktor.frontend

import dev.faktor.backend.BackendConnection
import dev.faktor.backend.BackendProcessManager
import dev.faktor.backend.NativeClient
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.MicroMoney
import dev.faktor.shared.NativeApiException
import dev.faktor.shared.NativeAttachmentId
import dev.faktor.shared.NativeCompletionContract
import dev.faktor.shared.NativeCompletionStepStatus
import dev.faktor.shared.NativeContractRows
import dev.faktor.shared.NativeTaskCompletion
import dev.faktor.shared.NativePermissionReplyRefusal
import dev.faktor.shared.NativeMessage
import dev.faktor.shared.NativeEvidenceRetrieval
import dev.faktor.shared.NativePermissionEntry
import dev.faktor.shared.NativeProtocolException
import dev.faktor.shared.NativeRequests
import dev.faktor.shared.NativeTerminalOutput
import dev.faktor.shared.ProtocolAttachmentRef
import dev.faktor.shared.parseNativeAgents
import dev.faktor.shared.parseNativeAttachmentId
import dev.faktor.shared.parseNativeBillingUsage
import dev.faktor.shared.parseNativeBoardPage
import dev.faktor.shared.parseNativeBoardPost
import dev.faktor.shared.parseNativeEntitlements
import dev.faktor.shared.parseNativeIdentity
import dev.faktor.shared.parseNativeModelCatalog
import dev.faktor.shared.parseNativeOrchestratorGraph
import dev.faktor.shared.parseNativePermissionList
import dev.faktor.shared.parseNativePresentationAck
import dev.faktor.shared.parseNativeSessionList
import dev.faktor.shared.parseNativeSessionUsage
import dev.faktor.shared.parseNativeTaskCompletionSteps
import dev.faktor.shared.parseNativeTaskProof
import dev.faktor.shared.indexCoverageLabel
import dev.faktor.shared.parseNativeIndexCoverage
import dev.faktor.shared.parseNativeTaskVerification
import dev.faktor.shared.parseNativeTaskViews
import dev.faktor.shared.parseNativeTerminalEventPage
import dev.faktor.shared.parseNativeTournament
import dev.faktor.shared.parseNativeTournamentDecision
import dev.faktor.shared.parseNativeTournamentStarted
import dev.faktor.shared.parseNativeTournamentSummaries
import dev.faktor.shared.parseNativeValueEntitlements
import dev.faktor.shared.parseNativeVerificationView
import dev.faktor.shared.parseProtocolAttachmentRef
import dev.faktor.shared.parseProtocolOutcomesPage
import dev.faktor.shared.view
import java.awt.GraphicsEnvironment
import java.awt.Toolkit
import java.awt.event.InputEvent
import java.awt.event.KeyEvent
import java.awt.image.BufferedImage
import java.io.ByteArrayInputStream
import java.math.BigInteger
import java.nio.file.Files
import java.nio.file.Paths
import javax.imageio.ImageIO
import javax.swing.KeyStroke
import javax.swing.tree.DefaultMutableTreeNode
import javax.swing.tree.TreePath

// ----------------------------------------------------------------- fixtures

private const val AGENTS_JSON = "[" +
    "{\"agent_id\":\"self-1\",\"kind\":\"self\",\"run_id\":\"run-1\"," +
    "\"session_id\":1,\"worktree_id\":1,\"goal\":\"ship it\",\"state\":\"Running\"," +
    "\"model\":\"m\",\"budget\":null,\"ownership\":\"self\",\"item_ids\":[\"main\"]," +
    "\"progress\":null,\"result\":null}," +
    "{\"agent_id\":\"child-1\",\"kind\":\"child\",\"run_id\":\"run-1\"," +
    "\"session_id\":9,\"worktree_id\":2,\"goal\":\"drive main step\",\"state\":\"Blocked\"," +
    "\"model\":\"m\",\"provider\":\"fake\",\"budget\":1000,\"ownership\":\"Mutating\"," +
    "\"item_id\":\"main\"," +
    "\"item_kind\":\"Implementation\"," +
    "\"blocker\":{\"kind\":\"permission\",\"reason\":\"shell call needs approval\"," +
    "\"dependency\":null,\"resolution\":\"allow the shell tool\"," +
    "\"last_progress_ms\":42}," +
    "\"capabilities\":[{\"cap\":\"ReadWorkspace\"}]," +
    "\"progress\":{\"lastOutputAt\":1,\"lastProgressAt\":2,\"lastOpCompletedAt\":3," +
    "\"inFlightOp\":null,\"silenceMs\":500,\"stallThresholdMs\":1000,\"stalled\":true}," +
    "\"result\":{\"summary\":\"main step output\",\"merge\":null}," +
    "\"presentation\":\"background\"}" +
    "]"

private const val TASK_JSON = "[" +
    "{\"goal\":\"ship it\",\"constraints\":[\"no network\"],\"state\":\"in_progress\"," +
    "\"milestones\":{\"completed\":[\"m1\"],\"open\":[\"m2\"]}," +
    "\"decisions\":[],\"failures\":[],\"changedFiles\":[\"a.rs\"]," +
    "\"tests\":{\"run\":[\"cargo test\"],\"failed\":[]}," +
    "\"preferences\":[],\"verification\":[],\"progress\":null," +
    "\"budget\":{\"maxTokens\":100,\"maxTurns\":5,\"spentTokens\":7,\"spentTurns\":1," +
    "\"maxCostMicro\":50,\"spentCostMicro\":2,\"openReservedMicro\":0}," +
    "\"acceptanceCriteria\":[\"criterion A\"]," +
    "\"plan\":[" +
    "{\"id\":\"step-1\",\"summary\":\"first\",\"state\":\"done\",\"depends_on\":[]}," +
    "{\"id\":\"step-2\",\"summary\":\"second\",\"state\":\"running\",\"depends_on\":[\"step-1\"]}" +
    "]," +
    "\"blockers\":[\"waiting on review\"]," +
    "\"evidenceRefs\":[\"evidence:41\"],\"phase\":\"building\"}" +
    "]"

// The additive engineering contract of one task
// (`crates/core/src/contract.rs`) as the task DTO serves it; TASK_JSON above
// predates it, so a legacy read stays an honest null.
private const val CONTRACT_TASK_JSON = "[" +
    "{\"goal\":\"ship it\",\"constraints\":[],\"state\":\"in_progress\"," +
    "\"milestones\":{\"completed\":[],\"open\":[]}," +
    "\"changedFiles\":[],\"tests\":{\"run\":[],\"failed\":[]},\"verification\":[]," +
    "\"contract\":{" +
    "\"constraints\":[{\"id\":\"c1\",\"statement\":\"existing sessions remain valid\"," +
    "\"origin\":\"user\"}]," +
    "\"invariants\":[{\"id\":\"i1\",\"statement\":\"revoked sessions cannot authenticate\"," +
    "\"origin\":{\"external\":{\"provider\":\"example.graph\"}}}]," +
    "\"risks\":[{\"id\":\"r1\",\"description\":\"race conditions\",\"severity\":\"high\"," +
    "\"origin\":\"heuristic\"}]," +
    "\"proof_obligations\":[" +
    "{\"id\":\"o1\",\"claim\":\"concurrent refresh produces one valid result\"," +
    "\"evidence\":\"test_execution\",\"origin\":\"user\",\"state\":\"supported\"," +
    "\"confidence_bps\":9100,\"evidence_refs\":[]}," +
    "{\"id\":\"o2\",\"claim\":\"replayed refresh tokens fail\"," +
    "\"evidence\":\"contract_check\",\"origin\":\"native\",\"state\":\"inferred\"," +
    "\"confidence_bps\":5000,\"evidence_refs\":[]}]}}]"

/** Malformed entries are dropped/disclosed; bad severity/state weaken. */
private const val CONTRACT_MALFORMED_JSON = "[" +
    "{\"goal\":\"ship it\",\"constraints\":[],\"state\":\"in_progress\"," +
    "\"milestones\":{\"completed\":[],\"open\":[]}," +
    "\"changedFiles\":[],\"tests\":{\"run\":[],\"failed\":[]},\"verification\":[]," +
    "\"contract\":{" +
    "\"constraints\":[{\"id\":\"c2\",\"statement\":\"\",\"origin\":\"user\"}," +
    "\"junk\"]," +
    "\"risks\":[{\"id\":\"r1\",\"description\":\"race\",\"severity\":\"catastrophic\"," +
    "\"origin\":\"heuristic\"}]," +
    "\"proof_obligations\":[{\"id\":\"o1\",\"claim\":\"ok\",\"evidence\":\"not_a_kind\"," +
    "\"origin\":\"native\",\"state\":\"maybe\",\"confidence_bps\":12000}]}}]"

/** A present non-object member is an explicit malformed read. */
private const val CONTRACT_UNREADABLE_JSON = "[" +
    "{\"goal\":\"ship it\",\"constraints\":[],\"state\":\"in_progress\"," +
    "\"milestones\":{\"completed\":[],\"open\":[]}," +
    "\"changedFiles\":[],\"tests\":{\"run\":[],\"failed\":[]},\"verification\":[]," +
    "\"contract\":\"nope\"}]"

private const val SESSION_USAGE_JSON = "{" +
    "\"sessionId\":\"1\",\"providerCalls\":{\"tokens\":7,\"prefixObservations\":[]}," +
    "\"prefixStability\":null,\"tasks\":[{\"taskId\":\"3\"," +
    "\"budget\":{\"maxTokens\":100,\"maxTurns\":5,\"spentTokens\":7,\"spentTurns\":1," +
    "\"maxCostMicro\":50,\"spentCostMicro\":2,\"openReservedMicro\":0}}]}"

private const val VERIFICATION_JSON = "{" +
    "\"owed\":[{\"opId\":\"op-3\",\"tool\":\"shell\",\"startedMs\":6," +
    "\"status\":\"running\",\"effectStatus\":\"unknown\"}]," +
    "\"failedChecks\":[]}"

// The strict proof summary (`faktor-task-proof/v1`) as `GET
// /native/tasks/{id}/proof` serves it: a fully VERIFIED story with criteria,
// required-check totals, review, verified==landed trees, the published commit
// + remote PR head and the spend fold.
private const val TASK_PROOF_JSON = "{" +
    "\"schema\":\"faktor-task-proof/v1\",\"sessionId\":\"1\",\"taskId\":\"3\"," +
    "\"proofState\":\"verified\",\"proofStateReason\":null," +
    "\"task\":{\"state\":\"verified_complete\",\"revision\":\"4\",\"goal\":\"ship it\"," +
    "\"updatedMs\":1,\"acceptanceCriteria\":[\"criterion A\"]}," +
    "\"criteria\":{\"recordId\":\"1\",\"recordStatus\":\"passed\",\"recordRevision\":\"3\"," +
    "\"certifiesCompletion\":true,\"total\":2,\"passed\":2,\"failed\":0," +
    "\"unavailable\":0,\"items\":[],\"truncated\":false}," +
    "\"checks\":{\"total\":3,\"passed\":3,\"failed\":0,\"other\":0,\"requiredTotal\":2," +
    "\"requiredPassed\":2,\"requiredFailed\":0,\"requiredAllPassed\":true," +
    "\"items\":[],\"truncated\":false}," +
    "\"review\":{\"recordId\":\"1\",\"status\":\"passed\"," +
    "\"reviewer\":{\"id\":\"review-bot\"},\"independentVerdict\":\"passed\"," +
    "\"independentCriteria\":[\"c2\"]}," +
    "\"trees\":{\"runBase\":\"tm1:aaa\",\"verified\":\"tm1:bbb\",\"landed\":\"tm1:bbb\"," +
    "\"landedEqualsVerified\":true,\"sourceCount\":1}," +
    "\"publication\":{\"verificationRecord\":\"1\",\"gitTreeOid\":null," +
    "\"commitOid\":\"2222222222222222222222222222222222222222\"," +
    "\"localRef\":\"refs/heads/main\"," +
    "\"remoteRef\":\"origin:refs/heads/main@2222222222222222222222222222222222222222\"," +
    "\"remoteHeadOid\":\"2222222222222222222222222222222222222222\"," +
    "\"pullRequest\":{\"provider\":\"github\"," +
    "\"operationKey\":\"task:3:rev:3:github:pull_request\",\"id\":\"pr-42\"," +
    "\"version\":\"2222222222222222222222222222222222222222@1\",\"headOid\":null," +
    "\"state\":\"completed\"}}," +
    "\"cost\":{\"status\":\"known\",\"reason\":null,\"spentCostMicro\":12," +
    "\"maxCostMicro\":1000000,\"openReservedMicro\":0,\"openReservations\":0," +
    "\"uncertainReservedMicro\":0,\"uncertainReservations\":0,\"settledCount\":1}," +
    "\"completion\":{\"contract\":{\"includeCommit\":false,\"includePush\":true," +
    "\"includePr\":true,\"revision\":\"3\",\"requestedSteps\":[\"push\",\"pr\"]}," +
    "\"steps\":[" +
    "{\"step\":\"push\",\"status\":\"succeeded\",\"present\":true," +
    "\"detail\":\"pushed to origin\",\"snapshot\":null,\"seq\":2,\"atMs\":1}," +
    "{\"step\":\"pr\",\"status\":\"succeeded\",\"present\":true," +
    "\"detail\":\"opened pr-42\",\"snapshot\":null,\"seq\":3,\"atMs\":2}]," +
    "\"records\":[],\"gate\":{\"status\":\"satisfied\",\"reason\":null}," +
    "\"truncated\":false}," +
    "\"integration\":{\"present\":true,\"inFlight\":false,\"runId\":\"run-1\"," +
    "\"finalSnapshotHash\":\"tm1:bbb\",\"landedSnapshot\":\"tm1:bbb\"," +
    "\"integratedFileCount\":1,\"conflictCount\":0," +
    "\"txn\":{\"txnId\":\"blake3:x\",\"phase\":\"landed\"}}," +
    "\"unavailable\":[]}"

// The same route with a VerifiedComplete task whose chain does not back it:
// the daemon verdict is `unavailable` and the component names the cause. The
// UI must render it explicitly and NEVER as VERIFIED.
private const val TASK_PROOF_UNAVAILABLE_JSON = "{" +
    "\"schema\":\"faktor-task-proof/v1\",\"sessionId\":\"1\",\"taskId\":\"3\"," +
    "\"proofState\":\"unavailable\"," +
    "\"proofStateReason\":\"landed_snapshot mismatch: the landed integration snapshot " +
    "does not equal the verified snapshot\"," +
    "\"task\":{\"state\":\"verified_complete\",\"revision\":\"4\",\"goal\":\"ship it\"," +
    "\"updatedMs\":1,\"acceptanceCriteria\":[\"criterion A\"]}," +
    "\"criteria\":{\"recordId\":\"1\",\"recordStatus\":\"passed\",\"recordRevision\":\"3\"," +
    "\"certifiesCompletion\":true,\"total\":2,\"passed\":2,\"failed\":0," +
    "\"unavailable\":0,\"items\":[],\"truncated\":false}," +
    "\"checks\":{\"total\":3,\"passed\":3,\"failed\":0,\"other\":0,\"requiredTotal\":2," +
    "\"requiredPassed\":2,\"requiredFailed\":0,\"requiredAllPassed\":true," +
    "\"items\":[],\"truncated\":false}," +
    "\"review\":{\"recordId\":\"1\",\"status\":\"passed\",\"reviewer\":{\"id\":\"review-bot\"}," +
    "\"independentVerdict\":\"passed\",\"independentCriteria\":[\"c2\"]}," +
    "\"trees\":{\"runBase\":\"tm1:aaa\",\"verified\":\"tm1:bbb\",\"landed\":\"tm1:ccc\"," +
    "\"landedEqualsVerified\":false,\"sourceCount\":1}," +
    "\"publication\":{\"verificationRecord\":\"1\",\"gitTreeOid\":null,\"commitOid\":null," +
    "\"localRef\":null,\"remoteRef\":null,\"remoteHeadOid\":null,\"pullRequest\":null}," +
    "\"cost\":{\"status\":\"unavailable\",\"reason\":\"budget read pool\"," +
    "\"spentCostMicro\":null,\"maxCostMicro\":null,\"openReservations\":null," +
    "\"settledCount\":null}," +
    "\"completion\":{\"contract\":null,\"steps\":[],\"records\":[]," +
    "\"gate\":{\"status\":\"no_contract\",\"reason\":null},\"truncated\":false}," +
    "\"integration\":{\"present\":true,\"inFlight\":false,\"runId\":\"run-1\"," +
    "\"finalSnapshotHash\":\"tm1:ccc\",\"landedSnapshot\":\"tm1:ccc\"," +
    "\"integratedFileCount\":0,\"conflictCount\":0,\"txn\":null}," +
    "\"unavailable\":[{\"component\":\"landed_snapshot\",\"kind\":\"mismatch\"," +
    "\"reason\":\"the landed integration snapshot does not equal the verified snapshot\"}]}"

// `GET /native/tasks/{id}/completion-steps`: one succeeded step, one MISSING
// step (no durable row) and the refused gate naming it.
private const val COMPLETION_STEPS_JSON = "{" +
    "\"schema\":\"faktor-task-completion-steps/v1\",\"sessionId\":\"1\",\"taskId\":\"3\"," +
    "\"contract\":{\"includeCommit\":false,\"includePush\":true,\"includePr\":true," +
    "\"revision\":\"3\",\"requestedSteps\":[\"push\",\"pr\"]}," +
    "\"steps\":[" +
    "{\"step\":\"push\",\"status\":\"succeeded\",\"present\":true," +
    "\"detail\":\"pushed to origin\",\"snapshot\":null,\"seq\":2,\"atMs\":1}," +
    "{\"step\":\"pr\",\"status\":\"missing\",\"present\":false," +
    "\"detail\":null,\"snapshot\":null,\"seq\":null,\"atMs\":null}]," +
    "\"records\":[]," +
    "\"gate\":{\"status\":\"refused\",\"reason\":\"step Pr has no durable status row\"}," +
    "\"truncated\":false}"

private const val TASK_VERIFICATION_JSON = "{" +
    "\"sessionId\":\"1\",\"taskId\":\"3\",\"records\":[{" +
    "\"recordId\":\"r1\",\"revision\":\"rev\",\"workspaceId\":\"1\",\"worktreeId\":\"1\"," +
    "\"treeHash\":null," +
    "\"criteria\":[{\"criterionKey\":\"c1\",\"passed\":true," +
    "\"evidence\":\"evidence:42 tool output\"}]," +
    "\"checks\":[{\"check\":\"unit\",\"program\":\"cargo\",\"args\":[\"test\"]," +
    "\"category\":\"test\",\"required\":true,\"status\":\"passed\",\"startedMs\":1," +
    "\"finishedMs\":2,\"exit\":0,\"summary\":\"see evidence:43\"}]," +
    "\"changedFiles\":[{\"path\":\"a.rs\",\"digestHex\":\"aa\",\"size\":3}]," +
    "\"unrelatedChanges\":[],\"reviewer\":\"review-bot\",\"status\":\"passed\"," +
    "\"startedMs\":1,\"completedMs\":2," +
    "\"candidateProof\":{\"taskRevision\":\"rev\",\"baseManifestHash\":null," +
    "\"candidateManifestHash\":null,\"sourceDiffEvidence\":null," +
    "\"riskReportEvidence\":null,\"accountingSnapshotDigest\":null," +
    "\"runId\":\"run-1\",\"runBaseSnapshot\":null,\"candidateSnapshot\":\"cand1234\"," +
    "\"sourcesDigest\":null,\"changedFilesDigest\":null," +
    "\"publishedCommit\":\"abcdef12\",\"remotePrHead\":\"refs/9\"}," +
    "\"verifiedSnapshot\":\"ver1234\",\"basedOnSnapshot\":\"base1234\"," +
    "\"sourceCount\":2,\"landedSnapshot\":\"land1234\"}]}"

// One criterion carrying FOUR served evidence refs: the criterion card must
// keep one bound button per ref (a west-stacked row used to drop all but the
// last).
private const val MULTI_EVIDENCE_VERIFICATION_JSON = "{" +
    "\"sessionId\":\"1\",\"taskId\":\"3\",\"records\":[{" +
    "\"recordId\":\"r1\",\"status\":\"passed\"," +
    "\"criteria\":[{\"criterionKey\":\"multi evidence\",\"passed\":true," +
    "\"evidence\":\"evidence:41 ref one; evidence:42 ref two; evidence:43 ref three; " +
    "evidence:44 ref four\"}]," +
    "\"checks\":[],\"changedFiles\":[]}]}"

// Criterion proof coverage: all SEVEN typed binding kinds served on the wire
// (the typed binding object + origin/requirement + the three-way verdict)
// plus a legacy unbound row, and the record-level P0 proof snapshots and
// timestamps. The model renders the served members directly: no fallback
// marker may appear for a member the wire carries.
private const val CRITERION_PROOF_JSON = "{" +
    "\"sessionId\":\"1\",\"taskId\":\"3\",\"records\":[{" +
    "\"recordId\":\"r1\",\"revision\":\"rev\",\"workspaceId\":\"1\",\"worktreeId\":\"1\"," +
    "\"treeHash\":null," +
    "\"criteria\":[" +
    "{\"criterionKey\":\"check criterion\",\"passed\":true," +
    "\"evidence\":\"check:rust_check:digest-check\"," +
    "\"binding\":{\"kind\":\"required_check\",\"check_id\":\"rust_check\"," +
    "\"command_digest\":\"digest-check\"}," +
    "\"origin\":\"user\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"coverage criterion\",\"passed\":true,\"evidence\":null," +
    "\"binding\":{\"kind\":\"integration_coverage\"," +
    "\"required_work_items\":[\"impl-a\",\"impl-b\"]}," +
    "\"origin\":\"project_policy\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"file criterion\",\"passed\":true,\"evidence\":\"file:src/a.rs\"," +
    "\"binding\":{\"kind\":\"file_state\",\"path\":\"src/a.rs\"," +
    "\"expected_digest\":\"digest-file\"}," +
    "\"origin\":\"verification_policy\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"evidence criterion\",\"passed\":true,\"evidence\":\"evidence:41\"," +
    "\"binding\":{\"kind\":\"evidence\",\"evidence_id\":\"41\"," +
    "\"evidence_digest\":\"digest-evidence\"}," +
    "\"origin\":\"user\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"review criterion\",\"passed\":true,\"evidence\":null," +
    "\"binding\":{\"kind\":\"independent_review\",\"reviewer_id\":\"reviewer-1\"}," +
    "\"origin\":\"project_policy\",\"requirement\":\"preferred\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"aggregate criterion\",\"passed\":true,\"evidence\":null," +
    "\"binding\":{\"kind\":\"aggregate_goal\"}," +
    "\"origin\":\"verification_policy\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"unavailable criterion\",\"passed\":false,\"evidence\":null," +
    "\"binding\":{\"kind\":\"unavailable\",\"reason\":\"no objective mechanism\"}," +
    "\"origin\":\"semantic_provider\",\"requirement\":\"required\"," +
    "\"verdict\":\"unavailable\"}," +
    "{\"criterionKey\":\"legacy criterion\",\"passed\":true," +
    "\"evidence\":\"file:src/legacy.rs\"}" +
    "]," +
    "\"checks\":[],\"changedFiles\":[],\"unrelatedChanges\":[],\"reviewer\":null," +
    "\"status\":\"passed\",\"startedMs\":11,\"completedMs\":22," +
    "\"candidateProof\":{\"taskRevision\":\"rev\",\"baseManifestHash\":\"base-manifest\"," +
    "\"candidateManifestHash\":\"cand-manifest\",\"sourceDiffEvidence\":null," +
    "\"riskReportEvidence\":null,\"accountingSnapshotDigest\":\"accounting:v1:feed\"," +
    "\"runId\":\"run-proof\",\"runBaseSnapshot\":\"base-snap\"," +
    "\"candidateSnapshot\":\"cand-snap\",\"sourcesDigest\":\"sources-digest\"," +
    "\"changedFilesDigest\":\"changed-digest\"," +
    "\"publishedCommit\":\"abcdef12\",\"remotePrHead\":\"refs/9\"}," +
    "\"verifiedSnapshot\":\"verified-snap\",\"basedOnSnapshot\":\"base-snap\"," +
    "\"sourceCount\":3,\"landedSnapshot\":\"landed-snap\"," +
    "\"reviewer\":\"review-bot\"" +
    "}]}"

private const val MODELS_JSON = "[" +
    "{\"provider\":\"fake\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
    "\"tools\":true,\"parallelTools\":false,\"reasoning\":true,\"thinking\":true," +
    "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
    "\"streaming\":true,\"source\":\"conservativeDefault\"}" +
    "]"

// Two providers exposing the SAME model id with different capability sets:
// the (provider, model) pair is the only safe catalog join key.
private const val DUAL_MODELS_JSON = "[" +
    "{\"provider\":\"alpha\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
    "\"tools\":true,\"parallelTools\":false,\"reasoning\":true,\"thinking\":false," +
    "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
    "\"streaming\":true,\"source\":\"conservativeDefault\"}," +
    "{\"provider\":\"beta\",\"model\":\"m\",\"context\":2000,\"maxOutput\":200," +
    "\"tools\":false,\"parallelTools\":false,\"reasoning\":false,\"thinking\":true," +
    "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
    "\"streaming\":true,\"source\":\"conservativeDefault\"}]"

private const val BOARD_PAGE_JSON = "{" +
    "\"board_id\":7,\"revision\":3,\"posts\":[" +
    "{\"id\":3,\"board_id\":7,\"author_child\":8,\"author_session\":8," +
    "\"subject\":\"handoff\",\"body\":\"main step ready\",\"refs\":[\"evidence:41\"]," +
    "\"revision\":3,\"created_ms\":1700}," +
    "{\"id\":2,\"board_id\":7,\"author_child\":null,\"author_session\":7," +
    "\"subject\":\"root note\",\"body\":\"no children yet\",\"refs\":[]," +
    "\"revision\":2,\"created_ms\":1600}" +
    "],\"next_before_revision\":2,\"has_more\":true}"

private const val BOARD_POST_JSON = "{" +
    "\"id\":3,\"board_id\":7,\"author_child\":8,\"author_session\":8," +
    "\"subject\":\"handoff\",\"body\":\"main step ready\",\"refs\":[]," +
    "\"revision\":3,\"created_ms\":1700}"

private const val GRAPH_JSON = "{" +
    "\"plan_id\":\"run-1\",\"goal\":\"graph goal\",\"state\":\"Running\"," +
    "\"work_items\":[" +
    "{\"item_id\":\"step-1\",\"kind\":\"Analysis\",\"state\":\"Done\"}," +
    "{\"item_id\":\"step-2\",\"kind\":\"Implementation\",\"state\":\"Running\"}]," +
    "\"children\":[{\"child_id\":\"child-1\",\"session_id\":9,\"operation_id\":1," +
    "\"worktree_id\":2,\"ownership\":\"Mutating\",\"state\":\"Blocked\"," +
    "\"blocker\":{\"kind\":\"dependency\",\"reason\":\"step-1 pending\"," +
    "\"dependency\":\"step-1\",\"resolution\":\"wait for step-1\",\"last_progress_ms\":5}," +
    "\"budget\":1000,\"capabilities\":[],\"plan_step_index\":1," +
    "\"steer_events\":[],\"merge\":null}]}"

private const val TOURNAMENT_JSON = "{" +
    "\"id\":\"t-1\",\"run_family\":\"run-7\",\"goal\":\"pick winner\"," +
    "\"criteria\":[{\"id\":\"c-1\",\"spec\":\"tests pass\"}]," +
    "\"candidates\":[" +
    "{\"child_id\":\"child-0\",\"worktree\":\"/tmp/w0\",\"base_revision\":\"abc\"," +
    "\"state\":\"done\",\"verification\":12,\"verification_pass\":true," +
    "\"review\":{\"rank\":\"clean\",\"reviewer\":\"rev-1\"}," +
    "\"cost_micro\":100,\"wall_ms\":1000}," +
    "{\"child_id\":\"child-1\",\"worktree\":\"/tmp/w1\",\"base_revision\":\"abc\"," +
    "\"state\":\"discarded\",\"verification\":null,\"verification_pass\":null," +
    "\"review\":null,\"cost_micro\":50,\"wall_ms\":900}]," +
    "\"winner\":\"child-0\",\"state\":\"decided\"}"

private const val TOURNAMENT_STARTED_JSON = "{" +
    "\"tournament_id\":\"t-1\",\"run_id\":\"run-7\"," +
    "\"candidates\":[\"child-0\",\"child-1\"],\"state\":\"open\",\"winner\":null}"

private const val TOURNAMENTS_LIST_JSON = "[" +
    "{\"id\":\"t-1\",\"state\":\"decided\",\"candidate_count\":2," +
    "\"winner\":\"child-0\",\"decided_ms\":123}," +
    "{\"id\":\"t-2\",\"state\":\"open\",\"candidate_count\":2," +
    "\"winner\":null,\"decided_ms\":null}]"

private const val TOURNAMENT_OPEN_JSON = "{" +
    "\"id\":\"t-2\",\"run_family\":\"run-8\",\"goal\":\"pick the open winner\"," +
    "\"criteria\":[{\"id\":\"c-1\",\"spec\":\"tests pass\"}]," +
    "\"candidates\":[" +
    "{\"child_id\":\"child-0\",\"worktree\":\"/tmp/w0\",\"base_revision\":\"abc\"," +
    "\"state\":\"done\",\"verification\":12,\"verification_pass\":true," +
    "\"review\":{\"rank\":\"clean\",\"reviewer\":\"rev-1\"}," +
    "\"cost_micro\":100,\"wall_ms\":1000}," +
    "{\"child_id\":\"child-1\",\"worktree\":\"/tmp/w1\",\"base_revision\":\"abc\"," +
    "\"state\":\"running\",\"verification\":null,\"verification_pass\":null," +
    "\"review\":null,\"cost_micro\":0,\"wall_ms\":0}]," +
    "\"winner\":null,\"state\":\"open\"}"

private const val TOURNAMENT_DECISION_JSON = "{" +
    "\"tournament_id\":\"t-2\",\"winner\":\"child-0\"," +
    "\"rationale\":\"winner child-0 (verification=pass)\"," +
    "\"discarded\":[{\"child_id\":\"child-1\",\"reason\":\"candidate ended failed\"}]}"

private const val PRESENTATION_ACK_JSON = "{" +
    "\"child_id\":\"child-1\",\"presentation\":\"background\",\"changed\":true}"

private const val PERMISSION_LIST_JSON = "{" +
    "\"permissions\":[{\"id\":\"7\",\"session_id\":\"9\",\"capability\":\"shell\"," +
    "\"detail\":{\"tool\":\"bash\"}}]}"

// T22: a served PR completion step carrying the daemon's typed
// `native_pr_scm_not_configured` blocker; the OTHER fixture is a PR failure
// with no such code (never read as a missing-SCM configuration).
private const val BLOCKED_PR_TASK_JSON = "[" +
    "{\"goal\":\"ship it\",\"state\":\"failed\",\"milestones\":{\"completed\":[],\"open\":[]}," +
    "\"changedFiles\":[],\"tests\":{\"run\":[],\"failed\":[]},\"verification\":[]," +
    "\"completion\":{\"contract\":{\"include_commit\":true,\"include_push\":true," +
    "\"include_pr\":true},\"steps\":[" +
    "{\"step\":\"push\",\"status\":\"succeeded\",\"detail\":\"pushed to origin\"}," +
    "{\"step\":\"pr\",\"status\":\"blocked\",\"detail\":\"native PR refused " +
    "(native_pr_scm_not_configured): the completion contract requests the native PR " +
    "step but no SCM provider is wired\"}]}}]"

private const val BLOCKED_PR_OTHER_JSON = "[" +
    "{\"goal\":\"ship it\",\"state\":\"failed\",\"milestones\":{\"completed\":[],\"open\":[]}," +
    "\"changedFiles\":[],\"tests\":{\"run\":[],\"failed\":[]},\"verification\":[]," +
    "\"completion\":{\"contract\":{\"include_commit\":true,\"include_push\":true," +
    "\"include_pr\":true},\"steps\":[" +
    "{\"step\":\"pr\",\"status\":\"failed\",\"detail\":\"remote rejected: authentication failed\"}" +
    "]}}]"

private const val SMOKE_SESSIONS_JSON = "{" +
    "\"sessions\":[{\"id\":\"7\",\"title\":\"work\",\"provider\":\"fake\",\"model\":\"m\"," +
    "\"state\":\"ready\",\"created_ms\":1700000000000}]}"

private const val BILLING_USAGE_JSON = "{" +
    "\"ok\":true,\"organization\":\"org-local\"," +
    "\"fold\":{\"organization_id\":\"org-local\"," +
    "\"totals\":{\"input_tokens\":1000,\"output_tokens\":500,\"cache_read_tokens\":200," +
    "\"cache_write_tokens\":50,\"reasoning_tokens\":25,\"provider_cost_micro\":900000," +
    "\"managed_cost_micro\":700000,\"byok_cost_micro\":200000,\"events\":12," +
    "\"corrected_events\":1}," +
    "\"per_task\":[{\"task_id\":3,\"run_id\":\"r1\"," +
    "\"totals\":{\"input_tokens\":400,\"output_tokens\":100,\"cache_read_tokens\":0," +
    "\"cache_write_tokens\":0,\"reasoning_tokens\":0,\"provider_cost_micro\":700000," +
    "\"managed_cost_micro\":700000,\"byok_cost_micro\":0,\"events\":3," +
    "\"corrected_events\":0}}],\"next_cursor\":null}," +
    "\"credits\":{\"granted_micro\":5000000,\"consumed_micro\":2000000," +
    "\"refunded_micro\":100000,\"held_micro\":250000,\"pending_consumes\":1}," +
    "\"items\":[{\"cursor\":\"9\",\"event\":{\"unit\":\"input_tokens\",\"quantity\":10}}]," +
    "\"nextCursor\":\"9\"" +
    "}"

private const val ENTITLEMENTS_JSON = "{" +
    "\"ok\":true,\"entitlements\":{" +
    "\"organization_id\":\"org-local\",\"billing_account_id\":\"acct-1\"," +
    "\"plan_id\":\"pro\",\"plan_found\":true,\"subscription_status\":\"active\"," +
    "\"subscription_expires_ms\":1800000000000,\"subscription_active\":true," +
    "\"features\":[\"managed_providers\",\"byok\",\"credits\"]," +
    "\"limits\":{\"max_tokens_per_period\":100000," +
    "\"max_managed_spend_micro_per_period\":1000000,\"min_credit_balance_micro\":1000," +
    "\"max_active_tasks\":4}," +
    "\"credits\":{\"granted_micro\":5000000,\"consumed_micro\":2000000," +
    "\"refunded_micro\":100000,\"held_micro\":250000,\"pending_consumes\":1}," +
    "\"managed_spend_micro\":700000,\"byok_spend_micro\":200000," +
    "\"total_tokens\":1775," +
    "\"in_flight\":[{\"id\":\"txn-1\",\"organization\":\"org-local\"," +
    "\"kind\":\"integration\",\"reference\":\"run-1\"," +
    "\"started_ms\":1750000000000,\"ended_ms\":null}]," +
    "\"now_ms\":1750000000000" +
    "}}"

private const val IDENTITY_JSON = "{" +
    "\"ok\":true,\"identity\":{\"subject_kind\":\"user\",\"subject_id\":\"u-1\"," +
    "\"display_name\":\"Admin\",\"email\":\"admin@example.com\"," +
    "\"organization\":\"org-local\",\"organization_name\":\"Local Org\"," +
    "\"role\":\"admin\",\"effective_actions\":[\"billing_read\",\"credits_grant\"]}" +
    "}"

private const val MEMBER_IDENTITY_JSON = "{" +
    "\"ok\":true,\"identity\":{\"subject_kind\":\"user\",\"subject_id\":\"u-2\"," +
    "\"display_name\":\"Member\",\"organization\":\"org-local\"," +
    "\"organization_name\":\"Local Org\",\"role\":\"member\"," +
    "\"effective_actions\":[\"billing_read\"]}" +
    "}"

// -------------------------------------------------------------------- smoke

/**
 * One exact-money billing usage payload: string `*_micro` money at the whole
 * i64::MAX range (the daemon's canonical form after the protocol change).
 */
private fun moneyUsageJson(managed: String, granted: String): String =
    "{\"ok\":true,\"organization\":\"org-local\"," +
        "\"fold\":{\"organization_id\":\"org-local\",\"totals\":{" +
        "\"input_tokens\":1,\"output_tokens\":0,\"cache_read_tokens\":0," +
        "\"cache_write_tokens\":0,\"reasoning_tokens\":0," +
        "\"provider_cost_micro\":\"$managed\",\"managed_cost_micro\":\"$managed\"," +
        "\"byok_cost_micro\":\"0\",\"events\":1,\"corrected_events\":0}," +
        "\"per_task\":[],\"next_cursor\":null}," +
        "\"credits\":{\"granted_micro\":\"$granted\",\"consumed_micro\":\"1\"," +
        "\"refunded_micro\":\"1\",\"held_micro\":\"0\",\"pending_consumes\":0}," +
        "\"items\":[],\"nextCursor\":null}"

/** One exact-money entitlement snapshot (string limits and spend). */
private fun moneyEntitlementsJson(managed: String, limit: String, floor: String): String =
    "{\"ok\":true,\"entitlements\":{\"organization_id\":\"org-local\"," +
        "\"plan_found\":true,\"subscription_active\":true,\"features\":[]," +
        "\"limits\":{\"max_managed_spend_micro_per_period\":\"$limit\"," +
        "\"min_credit_balance_micro\":\"$floor\"}," +
        "\"credits\":{\"granted_micro\":\"$managed\",\"consumed_micro\":\"1\"," +
        "\"refunded_micro\":\"1\",\"held_micro\":\"0\",\"pending_consumes\":0}," +
        "\"managed_spend_micro\":\"$managed\",\"byok_spend_micro\":\"0\"," +
        "\"total_tokens\":1,\"in_flight\":[],\"now_ms\":0}}"

object FrontendSmoke {

    private var failures = 0

    @JvmStatic
    fun main(args: Array<String>) {
        // I03/I07/I08 pure-surface tests (no display, no daemon) run first so
        // a model regression fails fast with its own name.
        changeReviewUnitTests()
        outcomesUnitTests()
        faktorSettingsUnitTests()
        step("canned native parsing: blocker/progress/result/capabilities") {
            val agents = parseNativeAgents(AGENTS_JSON)
            assertEquals(2, agents.size)
            val child = agents[1]
            assertEquals("child-1", child.agentId)
            assertEquals("main", child.itemId)
            assertEquals("Implementation", child.itemKind)
            assertEquals(1, child.capabilities.size)
            val blocker = child.blocker ?: fail("blocker must parse")
            assertEquals("permission", blocker.kind)
            assertEquals("shell call needs approval", blocker.reason)
            assertEquals("allow the shell tool", blocker.resolution)
            assertEquals(42L, blocker.lastProgressMs)
            val progress = child.progress ?: fail("progress must parse")
            assertEquals(true, progress.stalled)
            assertEquals(500L, progress.silenceMs)
            assertEquals("main step output", child.result?.summary)
            assertEquals(null, child.result?.merge)
        }

        step("canned native parsing: task view additive fields") {
            val task = parseNativeTaskViews(TASK_JSON)[0]
            assertEquals(listOf("criterion A"), task.acceptanceCriteria)
            assertEquals(2, task.plan.size)
            assertEquals(listOf("step-1"), task.plan[1].dependsOn)
            assertEquals(listOf("waiting on review"), task.blockers)
            assertEquals(listOf("evidence:41"), task.evidenceRefs)
            assertEquals("building", task.phase)
        }

        step("canned native parsing: index coverage names partial state honestly") {
            val partial = parseNativeIndexCoverage(INDEX_COVERAGE_JSON)
            assertEquals("partial", partial.snapshot!!.freshness)
            assertEquals(512L, partial.snapshot!!.coverage.filesIndexed)
            assertEquals(4500L, partial.snapshot!!.coverage.filesSeen)
            assertEquals("batch_files", partial.snapshot!!.coverage.truncatedReason)
            assertEquals(3, partial.snapshot!!.fingerprint.shard)
            assertEquals("index: partial 512/4500 files", indexCoverageLabel(partial.snapshot))
            // A never-hosted index service is reported honestly, never faked
            // as current.
            assertEquals(
                "index: not reported",
                indexCoverageLabel(parseNativeIndexCoverage(INDEX_COVERAGE_UNHOSTED_JSON).snapshot)
            )
        }

        step("canned native parsing: verification criteria/check summaries") {
            val verification = parseNativeTaskVerification(TASK_VERIFICATION_JSON)
            val record = verification.records[0]
            assertEquals("passed", record.status)
            assertEquals(1, record.criteriaPassed)
            assertEquals(1, record.criteriaTotal)
            assertEquals(1, record.criteria.size)
            assertEquals("evidence:42 tool output", record.criteria[0].evidence)
            assertEquals(listOf("see evidence:43"), record.checkSummaries)
            // The served proof family (snapshots, reviewer, published commit
            // and remote PR head) parses straight from the wire.
            assertEquals("ver1234", record.verifiedSnapshot)
            assertEquals("review-bot", record.reviewer)
            assertEquals("abcdef12", record.candidateProof?.publishedCommit)
            assertEquals("refs/9", record.candidateProof?.remotePrHead)
        }

        step("canned native parsing: tournament + tournament start receipt") {
            val tournament = parseNativeTournament(TOURNAMENT_JSON)
            assertEquals("t-1", tournament.id)
            assertEquals("run-7", tournament.runFamily)
            assertEquals("decided", tournament.state)
            assertEquals("child-0", tournament.winner)
            assertEquals(2, tournament.candidates.size)
            assertEquals("clean", tournament.candidates[0].reviewRank)
            assertEquals("rev-1", tournament.candidates[0].reviewer)
            assertEquals(12L, tournament.candidates[0].verification)
            assertEquals(true, tournament.candidates[0].verificationPass)
            val started = parseNativeTournamentStarted(TOURNAMENT_STARTED_JSON)
            assertEquals("t-1", started.tournamentId)
            assertEquals(listOf("child-0", "child-1"), started.candidates)
            assertEquals("open", started.state)
            assertEquals(null, started.winner)

            // The durable listing + decision ack + presentation ack parse.
            val summaries = parseNativeTournamentSummaries(TOURNAMENTS_LIST_JSON)
            assertEquals(2, summaries.size)
            assertEquals("t-1", summaries[0].id)
            assertEquals(2L, summaries[0].candidateCount)
            assertEquals("child-0", summaries[0].winner)
            assertEquals(123L, summaries[0].decidedMs)
            assertEquals(null, summaries[1].winner)
            assertEquals(null, summaries[1].decidedMs)
            assertEquals("t-2", latestTournamentId(summaries))
            val decision = parseNativeTournamentDecision(TOURNAMENT_DECISION_JSON)
            assertEquals("t-2", decision.tournamentId)
            assertEquals("child-0", decision.winner)
            assertEquals(1, decision.discarded.size)
            assertEquals("child-1", decision.discarded[0].childId)
            assertTrue(decision.rationale.contains("child-0"), "rationale must surface")
            val presentation = parseNativePresentationAck(PRESENTATION_ACK_JSON)
            assertEquals("child-1", presentation.childId)
            assertEquals("background", presentation.presentation)
            assertEquals(true, presentation.changed)
        }

        step("tournament panel gates decide on all candidates settled") {
            val runningOpen = TaskTree.tournamentView(parseNativeTournament(TOURNAMENT_OPEN_JSON))
            val panel = TournamentPanel()
            panel.setSummaries(parseNativeTournamentSummaries(TOURNAMENTS_LIST_JSON))
            assertTrue(panel.summariesText().contains("t-2[open]"), panel.summariesText())
            panel.setTournament(runningOpen)
            assertEquals(false, panel.decideEnabled())
            assertEquals(true, panel.abortEnabled())
            // All candidates settled: decide becomes available.
            val settled = runningOpen.copy(
                candidates = runningOpen.candidates.map { it.copy(state = "done") }
            )
            panel.setTournament(settled)
            assertEquals(true, panel.decideEnabled())
            assertEquals(true, panel.abortEnabled())
            // A decided tournament exposes no controls.
            panel.setTournament(TaskTree.tournamentView(parseNativeTournament(TOURNAMENT_JSON)))
            assertEquals(false, panel.decideEnabled())
            assertEquals(false, panel.abortEnabled())
            assertEquals(null, panel.current()?.takeIf { it.open })
        }

        step("canned native parsing: provider/reasoning catalog join + permissions") {
            val catalog = parseNativeModelCatalog(MODELS_JSON)
            assertEquals("fake", catalog[0].provider)
            assertEquals(true, catalog[0].reasoning)
            val permissions = parseNativePermissionList(PERMISSION_LIST_JSON)
            assertEquals("7", permissions[0].id)
            assertEquals("9", permissions[0].sessionId)
            assertEquals("shell", permissions[0].capability)
            assertTrue(permissions[0].detail.contains("bash"), "detail JSON must be kept")
        }

        step("attachment limits: advertised contract consumed, emergency ceiling from the shared fixture") {
            // The shared fixture is the Rust-checked daemon contract; the
            // JetBrains client consumes the advertised values and keeps the
            // conservative emergency ceiling only as a fallback.
            val root = System.getProperty("faktor.repo.root")
            val fixturePath =
                if (root.isNullOrBlank()) Paths.get("fixtures", "attachment-limits.json")
                else Paths.get(root, "fixtures", "attachment-limits.json")
            assertTrue(Files.exists(fixturePath), "shared fixture missing at $fixturePath")
            val fixture = JsonCodec.parse(Files.readString(fixturePath)).view("fixture")
            val canonical = fixture.field("canonical").rawJson()
            val emergency = fixture.field("emergencyCeiling")
            // A legacy daemon entry stays parseable (emergency fallback).
            val legacy = parseNativeModelCatalog(MODELS_JSON)
            assertEquals(null, legacy[0].attachmentLimits)
            assertEquals(
                "emergency",
                AttachmentImages.policyForModel(legacy, "fake", "m").source,
                "a legacy catalog must fall back to the emergency ceiling"
            )
            // The advertised entry parses and drives the policy.
            val advertisedJson = "[{\"provider\":\"fake\",\"model\":\"m\",\"context\":1000," +
                "\"maxOutput\":100,\"tools\":true,\"parallelTools\":false,\"reasoning\":true," +
                "\"thinking\":true,\"vision\":false,\"structuredOutput\":false," +
                "\"embeddings\":false,\"streaming\":true,\"source\":\"conservativeDefault\"," +
                "\"documentCapable\":true,\"attachmentLimits\":" + canonical + "}]"
            val catalog = parseNativeModelCatalog(advertisedJson)
            assertEquals(true, catalog[0].documentCapable)
            val limits = catalog[0].attachmentLimits ?: fail("advertised limits must parse")
            assertEquals(7340032L, limits.maxUploadBytes)
            assertEquals(5242880L, limits.image.mimes[0].maxBytes)
            assertEquals(8388608L, limits.document.mimes[0].maxBytes)
            val policy = AttachmentImages.policyForModel(catalog, "fake", "m")
            assertEquals("advertised", policy.source)
            assertEquals(7340032L, policy.maxUploadBytes)
            assertEquals(5242880L, policy.maxImageBytes)
            assertEquals(true, policy.documentCapable)
            assertEquals(listOf("application/pdf", "text/plain"), policy.documentMimes)
            // Unknown model / provider falls back to the emergency ceiling.
            assertEquals("emergency", AttachmentImages.policyForModel(catalog, "fake", "other").source)
            assertEquals("emergency", AttachmentImages.policyForModel(catalog, "other", "m").source)
            // The emergency ceiling is byte-identical to the fixture and
            // CONSERVATIVE: never above the advertised per-MIME bound.
            val emergencyPolicy = AttachmentImages.emergencyPolicy()
            assertEquals(emergency.field("maxUploadBytes").long(), emergencyPolicy.maxUploadBytes)
            assertEquals(emergency.field("maxImageBytes").long(), emergencyPolicy.maxImageBytes)
            assertEquals(emergency.field("maxDocumentBytes").long(), emergencyPolicy.maxDocumentBytes)
            assertEquals(AttachmentImages.MAX_IMAGE_BYTES, emergencyPolicy.maxImageBytes)
            assertEquals(AttachmentImages.MAX_UPLOAD_BYTES, emergencyPolicy.maxUploadBytes)
            assertTrue(
                emergencyPolicy.maxImageBytes <= policy.maxImageBytes,
                "the emergency ceiling must stay conservative"
            )
            // A tighter advertised bound flows into a tighter policy (and
            // the bounded read refuses above it).
            val tight = AttachmentImages.policyFromLimits(
                limits.copy(
                    image = limits.image.copy(
                        mimes = limits.image.mimes.map { it.copy(maxBytes = 3L) }
                    )
                )
            )
            assertEquals(3L, tight.maxImageBytes)
            val shot = Files.createTempFile("faktor-attach-limits-", ".png")
            Files.write(shot, byteArrayOf(1, 2, 3, 4))
            assertEquals(null, AttachmentImages.readBounded(shot.toFile(), tight.maxImageBytes))
            val read = AttachmentImages.readBounded(shot.toFile(), policy.maxImageBytes)
            assertTrue(read != null && read.size == 4, "the advertised bound reads byte-exact")
        }

        step("permission reply: typed 409 refusals classify and render explicitly") {
            val conflict = NativeApiException(
                409, "conflict", "permission 7 unknown or already resolved", false
            )
            assertTrue(NativePermissionReplyRefusal.isUnknownOrResolved(conflict))
            assertEquals(false, NativePermissionReplyRefusal.isSessionMismatch(conflict))
            val conflictText = NativePermissionReplyRefusal.describe(conflict, "7")
            assertTrue(
                conflictText != null && conflictText.contains("already resolved"),
                conflictText ?: "no conflict text"
            )
            val mismatch = NativeApiException(
                409, "permission_session_mismatch",
                "permission 7 is owned by session 8, not session 9", false
            )
            assertTrue(NativePermissionReplyRefusal.isSessionMismatch(mismatch))
            val mismatchText = NativePermissionReplyRefusal.describe(mismatch, "7")
            assertTrue(
                mismatchText != null && mismatchText.contains("different session"),
                mismatchText ?: "no mismatch text"
            )
            assertEquals(
                null,
                NativePermissionReplyRefusal.describe(
                    NativeApiException(409, "shadow_unregistered", "no shadow", false), "7"
                ),
                "unrelated 409 codes keep their own surface"
            )
            val panel = PermissionsPanel()
            panel.update(parseNativePermissionList(PERMISSION_LIST_JSON))
            panel.setReplyRefusal(conflictText!!)
            assertEquals(conflictText, panel.refusalText())
            assertTrue(panel.headerText().contains("last reply refused"), panel.headerText())
            assertEquals(1, panel.count(), "a refused reply keeps the pending list")
        }

        step("approval renderer: authority reason only when present, exact fallback + payload") {
            val withReason = NativePermissionEntry(
                "7", "9", "shell",
                "{\"tool\":\"bash\",\"reason\":\"the run needs the test runner to reproduce the failure\"}"
            )
            assertEquals(
                "the run needs the test runner to reproduce the failure",
                approvalAuthorityReason(withReason.detail)
            )
            assertEquals(
                "the run needs the test runner to reproduce the failure",
                approvalReasonText(withReason)
            )
            val bare = NativePermissionEntry("8", "9", "shell", "{\"tool\":\"bash\"}")
            assertEquals(
                null,
                approvalAuthorityReason(bare.detail),
                "a payload without reason/purpose/why must not yield an inferred purpose"
            )
            assertEquals("Action requested for this run.", approvalReasonText(bare))
            assertEquals("Run a shell command: bash", approvalHeadlineOf(bare))
            // A non-object detail carries no authority reason at all.
            assertEquals(null, approvalAuthorityReason("tool=bash reason=because"))
            assertEquals("Action requested for this run.", approvalReasonText(
                NativePermissionEntry("9", "9", "write", "just text")
            ))
            // The shared card renders the exact copy, the fallback and the
            // EXACT payload; the decisions are per-request Allow/Deny/Details.
            val gate = ApprovalReplyGate()
            val card = approvalRequestCard(bare, gate, onReply = {})
            val texts = textsOf(card.body)
            assertTrue(texts.contains("Action requested for this run."), "fallback missing: $texts")
            assertTrue(texts.contains("{\"tool\":\"bash\"}"), "exact payload missing: $texts")
            assertTrue(texts.any { it.contains("capability shell") }, "capability missing: $texts")
            assertTrue(texts.any { it.contains("target bash") }, "target missing: $texts")
            assertTrue(texts.contains("Run a shell command: bash"), "headline missing: $texts")
            assertEquals("Allow", card.allowButton.text)
            assertEquals("Deny", card.denyButton.text)
            assertEquals("Details", card.detailsButton.text)
            assertTrue(
                card.allowButton.accessibleContext.accessibleName
                    .contains("Allow approval request 8"),
                card.allowButton.accessibleContext.accessibleName
            )
            assertTrue(
                card.denyButton.accessibleContext.accessibleName
                    .contains("Deny approval request 8"),
                card.denyButton.accessibleContext.accessibleName
            )
            assertTrue(
                card.detailsButton.accessibleContext.accessibleName
                    .contains("approval request 8"),
                card.detailsButton.accessibleContext.accessibleName
            )
            // The authority reason replaces the fallback verbatim when present.
            val reasoned = approvalRequestCard(withReason, ApprovalReplyGate(), onReply = {})
            assertTrue(
                textsOf(reasoned.body).contains(
                    "the run needs the test runner to reproduce the failure"
                ),
                textsOf(reasoned.body).toString()
            )
        }

        step("approval gating: one reply per request id, refusal keeps the card, rebuild keeps pending") {
            val gate = ApprovalReplyGate()
            val workCard = WorkApprovalCard(gate)
            val replies = ArrayList<String>()
            var refreshes = 0
            workCard.setListener(object : WorkApprovalCard.Listener {
                override fun onReply(
                    permission: NativePermissionEntry,
                    decision: String
                ) {
                    replies.add("${permission.id}:$decision")
                }

                override fun onRefresh() {
                    refreshes++
                }
            })
            val permission = NativePermissionEntry("7", "9", "shell", "{\"tool\":\"bash\"}")
            workCard.update(listOf(permission))
            assertEquals(true, workCard.isVisible)
            assertEquals("Faktor needs your approval", workCard.headlineText())
            assertEquals("Review the action, target and stated reason.", workCard.supportingText())
            val buttons = workCard.replyButtonsForTest("7")
                ?: fail("the work approval card must render request 7")
            // A double click (the second on the now-disabled button) emits
            // exactly ONE reply.
            buttons.first.doClick()
            buttons.first.doClick()
            assertEquals(listOf("7:allow"), replies)
            assertEquals(false, buttons.first.isEnabled)
            assertEquals(false, buttons.second.isEnabled)
            assertEquals(true, gate.pending("7"))
            // A rebuild with the same pending snapshot keeps the request and
            // its in-flight gating (the state lives in the panel's gate, not
            // in the transient buttons).
            workCard.update(listOf(permission))
            val rebuilt = workCard.replyButtonsForTest("7")
                ?: fail("the rebuilt card must render request 7")
            assertTrue(rebuilt.first !== buttons.first, "the rebuild must create new buttons")
            assertEquals(false, rebuilt.first.isEnabled)
            assertEquals(false, rebuilt.second.isEnabled)
            // A typed refusal releases the gate, keeps the request visible
            // with its reason and offers a refresh; nothing is retried.
            gate.finish("7")
            val refusal =
                "permission 7 is unknown, expired or already resolved (409 conflict); refresh the pending list"
            workCard.setReplyRefusal("7", refusal)
            val released = workCard.replyButtonsForTest("7")
                ?: fail("the refused card must stay visible")
            assertEquals(true, released.first.isEnabled)
            assertEquals(true, released.second.isEnabled)
            assertEquals(refusal, workCard.refusalTextForTest("7"))
            val refusedTexts = textsOf(workCard)
            assertTrue(
                refusedTexts.any { it.contains("Reply refused: permission 7") },
                "the typed refusal must stay on the card: $refusedTexts"
            )
            assertEquals(0, refreshes)
            workCard.refreshButtonForTest("7")?.doClick()
                ?: fail("the refused card must offer Refresh")
            assertEquals(1, refreshes)
            // Success removes the card.
            workCard.update(emptyList())
            assertEquals(false, workCard.isVisible)
            assertEquals(0, workCard.count())

            // The SAME gate is shared with the Approvals list: a reply claimed
            // on one surface disables the request on the other.
            val sharedGate = ApprovalReplyGate()
            val work = WorkApprovalCard(sharedGate)
            val list = PermissionsPanel(sharedGate)
            val sharedReplies = ArrayList<String>()
            list.setListener(object : PermissionsPanel.Listener {
                override fun onPermissionReply(
                    permission: NativePermissionEntry,
                    decision: String
                ) {
                    sharedReplies.add(decision)
                }

                override fun onRefresh() {}
            })
            work.setListener(object : WorkApprovalCard.Listener {
                override fun onReply(
                    permission: NativePermissionEntry,
                    decision: String
                ) {
                    sharedReplies.add("work:$decision")
                }

                override fun onRefresh() {}
            })
            list.update(listOf(permission))
            work.update(listOf(permission))
            list.submitReply("deny")
            assertEquals(listOf("deny"), sharedReplies)
            assertEquals(true, sharedGate.pending("7"))
            // The work card rebuild sees the same in-flight id.
            work.update(listOf(permission))
            val cross = work.replyButtonsForTest("7") ?: fail("work card missing request 7")
            assertEquals(false, cross.first.isEnabled)
            assertEquals(false, cross.second.isEnabled)
            // The typed refusal is recorded on BOTH surfaces.
            sharedGate.finish("7")
            list.setReplyRefusal("7", refusal)
            work.setReplyRefusal("7", refusal)
            assertEquals(refusal, list.refusalText())
            assertEquals(refusal, work.refusalTextForTest("7"))
            assertTrue(list.allowEnabled(), "the refusal re-enables the list card")
            assertTrue(
                cross.first.isEnabled || work.replyButtonsForTest("7")!!.first.isEnabled,
                "the refusal re-enables the work card"
            )
        }

        step("approval announcement: once per new request, no focus jump") {
            val gate = ApprovalReplyGate()
            val card = WorkApprovalCard(gate)
            val announcements = ArrayList<String>()
            card.announcementSink = { text -> announcements.add(text) }
            val permission = NativePermissionEntry("7", "9", "shell", "{\"tool\":\"bash\"}")
            card.update(listOf(permission))
            assertEquals(1, announcements.size)
            assertTrue(announcements[0].startsWith("Faktor needs your approval"))
            assertTrue(announcements[0].contains("Run a shell command: bash"))
            // A rebuild with the same pending request announces nothing again.
            card.update(listOf(permission))
            assertEquals(1, announcements.size)
            // A genuinely new request is announced once.
            card.update(
                listOf(permission, NativePermissionEntry("8", "9", "network", "{\"host\":\"x\"}"))
            )
            assertEquals(2, announcements.size)
            // No approval path ever requests focus: the composer never moves.
            assertEquals(false, card.isFocusOwner)
        }

        step("utilities toolbar wraps at 240px with no horizontal scrollbar") {
            val service = FaktorFrontendService(
                Paths.get("unused-binary"),
                Files.createTempDirectory("faktor-toolbar-")
            )
            val panel = FaktorChatPanel(service)
            try {
                ParityAwt.layout(panel, 240, 600)
                val toolbar = panel.utilitiesToolbarForTest()
                assertTrue(
                    !containsScrollPane(toolbar),
                    "the toolbar must not carry a horizontal-scroll fallback"
                )
                assertTrue(toolbar.width <= 240, "toolbar width ${toolbar.width} exceeds 240")
                val rows = rowYs(toolbar)
                assertTrue(
                    rows.size >= 2,
                    "the toolbar must wrap into multiple rows at 240px (rows=$rows)"
                )
                for (child in (toolbar as java.awt.Container).components) {
                    if (!child.isVisible) continue
                    assertTrue(
                        child.x + child.width <= 240,
                        "'${child.javaClass.simpleName}' escapes the 240px host " +
                            "(x=${child.x} w=${child.width})"
                    )
                }
                // Primary controls stay visible: New session, Refresh and the
                // session chooser all render with real width.
                val labels = toolbar.components.mapNotNull {
                    when (it) {
                        is javax.swing.JButton -> it.text
                        is javax.swing.JComboBox<*> -> "session chooser"
                        else -> null
                    }
                }
                assertTrue(labels.contains("New session"), labels.toString())
                assertTrue(labels.contains("Refresh"), labels.toString())
                assertTrue(labels.contains("session chooser"), labels.toString())
                // The secondary utilities moved under More: no standalone
                // Settings button remains in the row.
                assertTrue(!labels.contains("Settings"), labels.toString())
            } finally {
                panel.shutdown()
            }
        }

        step("I06: opening the tool window never starts the daemon implicitly") {
            // Adversarial pin (VAL-HOST-5 / PROH-06 / F01 guard): the tool
            // window content path only CONSTRUCTS the service + panel
            // (FaktorToolWindowFactory createToolWindowContent does exactly
            // that; the kotlinc smoke compiles without the IntelliJ-platform
            // factory class). The resolver below is the service's start path:
            // any future implicit start during/after panel construction
            // resolves the daemon and fails this test.
            val startCalls = java.util.concurrent.atomic.AtomicInteger(0)
            val service = FaktorFrontendService(
                { startCalls.incrementAndGet(); Paths.get("unused-binary") },
                Files.createTempDirectory("faktor-no-implicit-start-")
            )
            val panel = FaktorChatPanel(service)
            try {
                assertEquals(
                    0,
                    startCalls.get(),
                    "opening the tool window must not resolve/start the daemon"
                )
                assertEquals(false, service.isRunning(), "no daemon may be running after open")
                assertEquals("stopped", service.daemonDescription())
                // The UI shows the stopped state until an explicit Start: the
                // start surface stays disabled and no readiness is claimed.
                assertEquals(false, panel.startTaskEnabledForTest())
                assertEquals(false, panel.newSessionEnabledForTest())
                assertEquals("—", panel.diagnosticsReadinessForTest())
                assertTrue(
                    !panel.transcriptTextForTest().contains("daemon ready"),
                    panel.transcriptTextForTest()
                )
                // ONLY the explicit Start service action reaches the start
                // path (the worker is FIFO, so the refresh drains the start).
                panel.startDaemon()
                assertTrue(panel.refreshNowForTest(10_000L), "the start job must finish")
                assertEquals(
                    1,
                    startCalls.get(),
                    "the explicit Start service action must invoke the start path exactly once"
                )
                assertEquals(false, service.isRunning())
            } finally {
                panel.shutdown()
            }
        }

        step("diagnostics: trust wording, readiness and T27/T28 copy pins") {
            assertEquals(
                "Matches recorded checksum",
                daemonTrustLabel(dev.faktor.backend.DaemonBinary.Trust.Verified("sha256 match"))
            )
            assertEquals(
                "No checksum record",
                daemonTrustLabel(dev.faktor.backend.DaemonBinary.Trust.Unverified("no record"))
            )
            assertEquals(
                "Checksum mismatch",
                daemonTrustLabel(dev.faktor.backend.DaemonBinary.Trust.Refused("mismatch"))
            )
            assertEquals("—", daemonTrustLabel(null))
            val labels = listOf(
                daemonTrustLabel(dev.faktor.backend.DaemonBinary.Trust.Verified("x")),
                daemonTrustLabel(dev.faktor.backend.DaemonBinary.Trust.Unverified("x")),
                daemonTrustLabel(dev.faktor.backend.DaemonBinary.Trust.Refused("x"))
            )
            assertTrue(
                labels.none { it.contains("signed", ignoreCase = true) },
                "the bare .sha256 record must never be worded as signed: $labels"
            )
            val panel = DiagnosticsPanel()
            panel.setTrust(
                daemonTrustLabel(dev.faktor.backend.DaemonBinary.Trust.Verified("x")),
                "/opt/faktor/launcher"
            )
            assertEquals("Matches recorded checksum\n/opt/faktor/launcher", panel.trustText())
            panel.setTrust("—", null)
            assertEquals("—", panel.trustText())
            // Readiness: waiting / ready / timeout, never 0.
            assertEquals("—", panel.readinessText())
            panel.setReadiness("waiting", "starting the service")
            assertTrue(panel.readinessText().startsWith("waiting"), panel.readinessText())
            panel.setReadiness("ready", "version 1.2.3")
            assertTrue(panel.readinessText().startsWith("ready"), panel.readinessText())
            panel.setReadiness("timeout", "daemon never reported ready")
            assertTrue(panel.readinessText().startsWith("timeout"), panel.readinessText())
            assertTrue(panel.readinessText().contains("never reported ready"))
            // T27 verbatim copy + actions.
            panel.setStreamRecovery(StreamRecoveryState.RECONNECTING)
            assertEquals("Reconnecting live updates", panel.recoveryHeadline())
            assertEquals("The last valid journal position is kept.", panel.recoveryBody())
            assertEquals(listOf("Wait", "Refresh"), panel.recoveryActions())
            // T28 verbatim copy + actions; a skip action never exists.
            panel.setStreamRecovery(
                StreamRecoveryState.PAUSED,
                "schema_mismatch at cursor 42: unsupported frame schema"
            )
            assertEquals("Live updates paused", panel.recoveryHeadline())
            assertEquals(
                "The client cannot read the next durable event. Nothing is skipped.",
                panel.recoveryBody()
            )
            assertEquals(listOf("Reconnect", "Refresh from snapshot"), panel.recoveryActions())
            assertTrue(
                panel.recoveryActions().none { it.contains("skip", ignoreCase = true) },
                "recovery must never offer a skip action"
            )
            // The cursor readout stays on the Live updates row.
            panel.setConnection("connected: http://127.0.0.1:9", "retrying", 42L, "7")
            assertTrue(panel.streamText().contains("cursor 42"), panel.streamText())
        }

        step("diagnostics setup: T32 copy + exact sandbox guidance command") {
            assertEquals("Shell execution needs a decision", SANDBOX_SETUP_HEADLINE)
            assertEquals(
                "The secure default remains fail-closed on this platform.",
                SANDBOX_SETUP_COPY
            )
            assertEquals("Open setup guidance", SANDBOX_SETUP_ACTION)
            assertEquals("faktor sandbox choose", SANDBOX_GUIDANCE_COMMAND)
            val panel = DiagnosticsPanel()
            val texts = textsOf(panel)
            assertTrue(texts.contains("Shell execution needs a decision"), texts.toString())
            assertTrue(
                texts.contains("The secure default remains fail-closed on this platform."),
                texts.toString()
            )
            assertTrue(texts.contains("faktor sandbox choose"), texts.toString())
            assertEquals("faktor sandbox choose", panel.sandboxCommandTextForTest())
            assertEquals("Open setup guidance", panel.setupActionForTest().text)
            assertEquals(false, panel.setupGuidanceVisibleForTest())
            panel.setupActionForTest().doClick()
            assertEquals(true, panel.setupGuidanceVisibleForTest())
        }

        step("T22: served SCM-not-configured PR block surfaces the exact copy") {
            val blocked = parseNativeTaskViews(BLOCKED_PR_TASK_JSON)[0]
            assertTrue(prScmSetupBlocked(blocked), "the typed blocker code must be detected")
            assertEquals(false, prScmSetupBlocked(parseNativeTaskViews(TASK_JSON)[0]))
            assertEquals(false, prScmSetupBlocked(null))
            // A PR step with any OTHER failure is not read as missing SCM.
            val other = parseNativeTaskViews(BLOCKED_PR_OTHER_JSON)[0]
            assertEquals(false, prScmSetupBlocked(other))
            // The Work current-run strip renders the exact T22 copy and the
            // setup-guidance action only for the typed blocker.
            val service = FaktorFrontendService(
                Paths.get("unused-binary"),
                Files.createTempDirectory("faktor-t22-")
            )
            val panel = FaktorChatPanel(service)
            try {
                panel.applyRunSummaryForTest(parseNativeTaskViews(TASK_JSON)[0])
                assertEquals(false, panel.prSetupNoticeVisibleForTest())
                panel.applyRunSummaryForTest(blocked)
                assertEquals(true, panel.prSetupNoticeVisibleForTest())
                val notice = panel.prSetupCopyForTest()
                assertEquals("Pull request integration unavailable", notice.first)
                assertEquals(
                    "Configure the native SCM integration; no PR success is inferred.",
                    notice.second
                )
                assertEquals("Open setup guidance", notice.third)
                panel.applyRunSummaryForTest(null)
                assertEquals(false, panel.prSetupNoticeVisibleForTest())
            } finally {
                panel.shutdown()
            }
        }

        step("run-state copy: T06-T26 exact labels render only under their served conditions") {
            // Verbatim transcription guards: the state-matrix copy is exact.
            assertEquals("Ready", RunStateCopy.READY_LABEL)
            assertEquals("Describe a task or ask a question.", RunStateCopy.READY_COPY)
            assertEquals("Run task", RunStateCopy.READY_ACTION)
            assertEquals("Starting the run", RunStateCopy.STARTING_LABEL)
            assertEquals("Your captured draft is being admitted once.", RunStateCopy.STARTING_COPY)
            assertEquals("Wait", RunStateCopy.STARTING_ACTION)
            assertEquals(
                "Start response unavailable",
                RunStateCopy.START_RESPONSE_UNAVAILABLE_LABEL
            )
            assertEquals(
                "The draft was kept. Retry checks the same logical submission.",
                RunStateCopy.START_RESPONSE_UNAVAILABLE_COPY
            )
            assertEquals("Review and retry", RunStateCopy.START_RESPONSE_UNAVAILABLE_ACTION)
            assertEquals("Running", RunStateCopy.RUNNING_LABEL)
            assertEquals("The candidate is separate from your workspace.", RunStateCopy.RUNNING_COPY)
            assertEquals("Inspect / Stop current run", RunStateCopy.RUNNING_ACTION)
            assertEquals("Needs your input", RunStateCopy.NEEDS_INPUT_LABEL)
            assertEquals(
                "The run is waiting for a decision or clarification.",
                RunStateCopy.NEEDS_INPUT_COPY
            )
            assertEquals("Reply in composer", RunStateCopy.NEEDS_INPUT_ACTION)
            assertEquals("Workspace changed", RunStateCopy.WORKSPACE_CHANGED_LABEL)
            assertEquals(
                "The current workspace no longer matches the integration basis.",
                RunStateCopy.WORKSPACE_CHANGED_COPY
            )
            assertEquals("Inspect conflict", RunStateCopy.WORKSPACE_CHANGED_ACTION)
            assertEquals("Finish action pending", RunStateCopy.FINISH_ACTION_PENDING_LABEL)
            assertEquals(
                "A requested action lacks a succeeded durable result.",
                RunStateCopy.FINISH_ACTION_PENDING_COPY
            )
            assertEquals("View finish actions", RunStateCopy.FINISH_ACTION_PENDING_ACTION)
            assertEquals("Run cancelled", RunStateCopy.RUN_CANCELLED_LABEL)
            assertEquals(
                "Cancellation is a terminal run outcome, not verification success.",
                RunStateCopy.RUN_CANCELLED_COPY
            )
            assertEquals(
                "Review recorded work / New task",
                RunStateCopy.RUN_CANCELLED_ACTION
            )
            assertEquals("Run failed", RunStateCopy.RUN_FAILED_LABEL)
            assertEquals(
                "The result and reason remain available for inspection.",
                RunStateCopy.RUN_FAILED_COPY
            )
            assertEquals("Inspect result", RunStateCopy.RUN_FAILED_ACTION)
            assertEquals("Checking the change", RunStateCopy.CHECKING_LABEL)
            assertEquals(
                "Required results remain attached to this candidate.",
                RunStateCopy.CHECKING_COPY
            )
            assertEquals("Inspect verification", RunStateCopy.CHECKING_ACTION)

            // T07/T08: the client start lifecycle, fresher than any served
            // snapshot. T07 `Wait` has no affordance: label+copy only, so the
            // action never renders (null in the model).
            val starting = runStateViewForStart(inFlight = true, submissionRetained = false)
                ?: fail("a start in flight must be the T07 state")
            assertEquals(RunStateCopy.STARTING_LABEL, starting.label)
            assertEquals(RunStateCopy.STARTING_COPY, starting.copy)
            assertEquals(null, starting.action)
            assertEquals(null, runStateViewForStart(inFlight = false, submissionRetained = false))
            val lost = runStateViewForStart(inFlight = false, submissionRetained = true)
                ?: fail("a retained submission must be the T08 state")
            assertEquals(RunStateCopy.START_RESPONSE_UNAVAILABLE_LABEL, lost.label)
            assertEquals(RunStateCopy.START_RESPONSE_UNAVAILABLE_COPY, lost.copy)
            assertEquals(RunStateCopy.START_RESPONSE_UNAVAILABLE_ACTION, lost.action)

            // The served task-run state tags: only Running / Blocked / Failed /
            // Cancelled map; Done / Pending / Unknown stay null (the strip
            // then renders its served facts only).
            assertEquals(
                RunStateCopy.RUNNING_LABEL,
                runStateViewForRunTag("Running")?.label
            )
            assertEquals(
                RunStateCopy.NEEDS_INPUT_LABEL,
                runStateViewForRunTag("Blocked")?.label
            )
            assertEquals(
                RunStateCopy.RUN_FAILED_LABEL,
                runStateViewForRunTag("Failed")?.label
            )
            assertEquals(
                RunStateCopy.RUN_CANCELLED_LABEL,
                runStateViewForRunTag("cancelled")?.label
            )
            for (tag in listOf("Done", "Pending", "Unknown", null)) {
                assertEquals(null, runStateViewForRunTag(tag))
            }

            // T12: the proof's served `task.state` is the ONLY source of the
            // checking state. `verifying` / `needs_verification` (the
            // daemon's TaskState labels) map; every other served state — or
            // none — stays null, never fabricated into a checking state.
            for (state in listOf("verifying", "needs_verification")) {
                val checking = runStateViewForProofTaskState(state)
                    ?: fail("$state must render the T12 checking state")
                assertEquals(RunStateCopy.CHECKING_LABEL, checking.label)
                assertEquals(RunStateCopy.CHECKING_COPY, checking.copy)
                assertEquals(RunStateCopy.CHECKING_ACTION, checking.action)
            }
            for (state in listOf(null, "", "running", "failed", "verified_complete")) {
                assertEquals(null, runStateViewForProofTaskState(state))
            }
            // Precedence: the client start lifecycle wins; a settled or
            // awaiting run tag (Failed / Cancelled / Blocked) outranks T12;
            // T12 refines the collapsed Running tag; Done / Pending / Unknown
            // carry no design state and let the proof's T12 render.
            assertEquals(
                RunStateCopy.STARTING_LABEL,
                runStateViewForRun(true, false, "Running", "verifying")?.label
            )
            assertEquals(
                RunStateCopy.START_RESPONSE_UNAVAILABLE_LABEL,
                runStateViewForRun(false, true, "Running", "verifying")?.label
            )
            assertEquals(
                RunStateCopy.RUN_FAILED_LABEL,
                runStateViewForRun(false, false, "Failed", "verifying")?.label
            )
            assertEquals(
                RunStateCopy.RUN_CANCELLED_LABEL,
                runStateViewForRun(false, false, "Cancelled", "needs_verification")?.label
            )
            assertEquals(
                RunStateCopy.NEEDS_INPUT_LABEL,
                runStateViewForRun(false, false, "Blocked", "verifying")?.label
            )
            for (state in listOf("verifying", "needs_verification")) {
                assertEquals(
                    RunStateCopy.CHECKING_LABEL,
                    runStateViewForRun(false, false, "Running", state)?.label
                )
                assertEquals(
                    RunStateCopy.CHECKING_LABEL,
                    runStateViewForRun(false, false, "Done", state)?.label
                )
            }
            assertEquals(
                RunStateCopy.RUNNING_LABEL,
                runStateViewForRun(false, false, "Running", null)?.label
            )
            assertEquals(null, runStateViewForRun(false, false, "Done", null))
            assertEquals(null, runStateViewForRun(false, false, null, null))
            // Negative: the collapsed run tags ALONE never render T12 — with
            // no served proof state behind them they keep their own design
            // state (or none).
            for (tag in listOf(
                "Running",
                "Blocked",
                "Failed",
                "Cancelled",
                "Done",
                "Pending",
                "Unknown"
            )) {
                val tagOnly = runStateViewForRun(false, false, tag, null)
                assertTrue(
                    tagOnly == null || tagOnly.label != RunStateCopy.CHECKING_LABEL,
                    "the run tag $tag alone must never render T12"
                )
            }

            // The additive proof `task` member: `task.state` is parsed when
            // served; an absent or JSON-null member stays null (nothing is
            // fabricated) and a present-but-malformed member refuses typed
            // like its sibling proof members.
            val verifyingProof = parseNativeTaskProof(
                TASK_PROOF_JSON.replace(
                    "\"state\":\"verified_complete\"",
                    "\"state\":\"verifying\""
                )
            )
            assertEquals("verifying", verifyingProof.taskState)
            assertEquals(
                null,
                parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(Regex("\"task\":\\{.*?\\},"), "")
                ).taskState,
                "an absent task member must stay null"
            )
            assertEquals(
                null,
                parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(Regex("\"task\":\\{.*?\\},"), "\"task\":null,")
                ).taskState,
                "a JSON-null task member must stay null"
            )
            var taskStateRefused = false
            try {
                parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(
                        "\"state\":\"verified_complete\"",
                        "\"state\":7"
                    )
                )
            } catch (e: NativeProtocolException) {
                taskStateRefused = true
                assertTrue(
                    e.path.contains("task.state"),
                    "the typed refusal must name task.state: ${e.path}"
                )
            }
            assertTrue(taskStateRefused, "a malformed task.state must refuse typed")
            var taskMemberRefused = false
            try {
                parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(
                        Regex("\"task\":\\{.*?\\},"),
                        "\"task\":\"verifying\","
                    )
                )
            } catch (e: NativeProtocolException) {
                taskMemberRefused = true
            }
            assertTrue(taskMemberRefused, "a non-object task member must refuse typed")

            val service = FaktorFrontendService(
                Paths.get("unused-binary"),
                Files.createTempDirectory("faktor-runstates-")
            )
            val panel = FaktorChatPanel(service)
            try {
                // T06: the real composer empty state renders the exact
                // guidance and hides it as soon as a draft exists; the
                // Task/Ask mode control and the options sheet stay intact.
                val ready = panel.composerReadyForTest()
                    ?: fail("the empty composer must render its ready guidance")
                assertEquals(RunStateCopy.READY_LABEL, ready.first)
                assertEquals(RunStateCopy.READY_COPY, ready.second)
                val controls = panel.actionControlsForTest()
                assertEquals(RunStateCopy.READY_ACTION, controls["runTask"])
                assertEquals("Cancel run", controls["stopRun"])
                assertEquals("Inspect", controls["inspect"])
                assertEquals("New session", controls["newSession"])
                panel.setTaskFieldsForTest("a draft", "")
                assertEquals(null, panel.composerReadyForTest(), "a draft must end the ready state")
                panel.setTaskFieldsForTest("", "")
                assertEquals(
                    RunStateCopy.READY_COPY,
                    panel.composerReadyForTest()?.second,
                    "clearing the draft restores the ready state"
                )
                assertEquals(false, panel.askModeSelected(), "Task mode stays the default")
                panel.selectModeForTest(true)
                assertEquals(true, panel.askModeSelected(), "the Ask mode control must keep working")
                panel.selectModeForTest(false)
                panel.openTaskOptionsForTest()
                assertEquals(true, panel.taskOptionsOpenForTest())
                panel.backTaskOptionsForTest()
                assertEquals(false, panel.taskOptionsOpenForTest())

                // The strip renders each served run state with its exact
                // label+copy; states that are not served never bleed through.
                val task = parseNativeTaskViews(TASK_JSON)[0]
                panel.applyRunSummaryForTest(task, "Running")
                val running = panel.runSummaryTextForTest()
                assertTrue(
                    running.startsWith(
                        RunStateCopy.RUNNING_LABEL + "\n" + RunStateCopy.RUNNING_COPY
                    ),
                    running
                )
                assertTrue(running.contains("goal: ship it"), running)
                assertEquals(
                    RunStateCopy.RUNNING_LABEL,
                    panel.runStatusBannerForTest()?.label
                )
                panel.applyRunSummaryForTest(task, "Blocked")
                val blocked = panel.runSummaryTextForTest()
                assertTrue(blocked.startsWith(RunStateCopy.NEEDS_INPUT_LABEL), blocked)
                assertTrue(blocked.contains(RunStateCopy.NEEDS_INPUT_COPY), blocked)
                assertTrue(
                    !blocked.contains(RunStateCopy.RUNNING_COPY),
                    "a Blocked run is not Running: $blocked"
                )
                panel.applyRunSummaryForTest(task, "Cancelled")
                val cancelled = panel.runSummaryTextForTest()
                assertTrue(cancelled.startsWith(RunStateCopy.RUN_CANCELLED_LABEL), cancelled)
                assertTrue(cancelled.contains(RunStateCopy.RUN_CANCELLED_COPY), cancelled)
                assertTrue(
                    !cancelled.contains(RunStateCopy.RUN_CANCELLED_ACTION),
                    "the T25 action has no control here, so it never renders: $cancelled"
                )
                panel.applyRunSummaryForTest(task, "Failed")
                val failed = panel.runSummaryTextForTest()
                assertTrue(failed.startsWith(RunStateCopy.RUN_FAILED_LABEL), failed)
                assertTrue(failed.contains(RunStateCopy.RUN_FAILED_COPY), failed)
                // Negative: a Done run carries no design run state at all.
                panel.applyRunSummaryForTest(task, "Done")
                val done = panel.runSummaryTextForTest()
                for (label in listOf(
                    RunStateCopy.RUNNING_LABEL,
                    RunStateCopy.NEEDS_INPUT_LABEL,
                    RunStateCopy.RUN_CANCELLED_LABEL,
                    RunStateCopy.RUN_FAILED_LABEL,
                    RunStateCopy.STARTING_LABEL,
                    RunStateCopy.START_RESPONSE_UNAVAILABLE_LABEL
                )) {
                    assertTrue(!done.contains(label), "Done must not render '$label': $done")
                }
                assertTrue(done.contains("goal: ship it"), done)

                // T12: the proof's served task state (verifying /
                // needs_verification) refines the collapsed Running tag into
                // the exact checking label+copy. The collapsed Running copy
                // never bleeds in, the existing Inspect control stays the
                // affordance (no button is invented) and the design action
                // renders only through that existing control.
                for (state in listOf("verifying", "needs_verification")) {
                    val proof = parseNativeTaskProof(
                        TASK_PROOF_JSON.replace(
                            "\"state\":\"verified_complete\"",
                            "\"state\":\"" + state + "\""
                        )
                    )
                    assertEquals(state, proof.taskState)
                    panel.applyRunSummaryForTest(task, "Running", proof.taskState)
                    val checking = panel.runSummaryTextForTest()
                    assertTrue(
                        checking.startsWith(
                            RunStateCopy.CHECKING_LABEL + "\n" + RunStateCopy.CHECKING_COPY
                        ),
                        "$state must render the exact T12 banner: $checking"
                    )
                    assertTrue(
                        !checking.contains(RunStateCopy.RUNNING_COPY),
                        "the collapsed Running copy must not bleed into T12: $checking"
                    )
                    assertEquals(
                        RunStateCopy.CHECKING_LABEL,
                        panel.runStatusBannerForTest()?.label
                    )
                    assertEquals(
                        RunStateCopy.CHECKING_ACTION,
                        panel.runStatusBannerForTest()?.action
                    )
                    assertEquals(
                        "Inspect",
                        panel.actionControlsForTest()["inspect"],
                        "T12's affordance is the EXISTING Inspect control"
                    )
                }
                // An absent/refused proof task state fabricates nothing: the
                // served Running tag keeps leading the strip and the checking
                // label never renders from the tag alone.
                panel.applyRunSummaryForTest(task, "Running", null)
                val tagOnlyStrip = panel.runSummaryTextForTest()
                assertTrue(
                    tagOnlyStrip.startsWith(
                        RunStateCopy.RUNNING_LABEL + "\n" + RunStateCopy.RUNNING_COPY
                    ),
                    "without a served proof task state the tag keeps T09: $tagOnlyStrip"
                )
                assertTrue(
                    !tagOnlyStrip.contains(RunStateCopy.CHECKING_LABEL),
                    "the collapsed tag alone never renders T12: $tagOnlyStrip"
                )

                // T21: a requested finish action without a succeeded durable
                // result renders the exact label+copy ON the completion row;
                // an all-succeeded completion never renders it.
                val pendingCompletion = task.copy(
                    completion = NativeTaskCompletion(
                        NativeCompletionContract(true, true, false),
                        listOf(
                            NativeCompletionStepStatus("commit", "pending", ""),
                            NativeCompletionStepStatus("push", "succeeded", "pushed")
                        )
                    )
                )
                val pendingModel = TaskTree.build(task = pendingCompletion)
                assertEquals(true, pendingModel.completion?.finishActionPending)
                val overview = OverviewPanel()
                overview.updateModel(pendingModel)
                val finishRow = overview.summaryText("Finish actions")
                assertTrue(
                    finishRow.startsWith(
                        RunStateCopy.FINISH_ACTION_PENDING_LABEL + " — " +
                            RunStateCopy.FINISH_ACTION_PENDING_COPY
                    ),
                    finishRow
                )
                assertTrue(finishRow.contains("commit pending"), finishRow)
                val doneCompletion = task.copy(
                    completion = NativeTaskCompletion(
                        NativeCompletionContract(true, true, false),
                        listOf(
                            NativeCompletionStepStatus("commit", "succeeded", "committed"),
                            NativeCompletionStepStatus("push", "succeeded", "pushed")
                        )
                    )
                )
                val doneModel = TaskTree.build(task = doneCompletion)
                assertEquals(false, doneModel.completion?.finishActionPending)
                overview.updateModel(doneModel)
                val doneRow = overview.summaryText("Finish actions")
                assertTrue(
                    !doneRow.contains(RunStateCopy.FINISH_ACTION_PENDING_LABEL),
                    doneRow
                )
                assertTrue(doneRow.contains("commit succeeded"), doneRow)
                // The derived rows of a still-running contract are pending
                // too; the durable gate-certified (done) rows are not.
                assertEquals(
                    true,
                    TaskTree.build(
                        task = task,
                        submittedCompletion = NativeCompletionContract(true, false, false),
                        runState = "running"
                    ).completion?.finishActionPending
                )
                assertEquals(
                    false,
                    TaskTree.build(
                        task = task,
                        submittedCompletion = NativeCompletionContract(true, false, false),
                        runState = "done"
                    ).completion?.finishActionPending
                )
                // A requested step with NO row at all is pending as well.
                assertEquals(
                    true,
                    TaskTree.build(
                        task = task.copy(
                            completion = NativeTaskCompletion(
                                NativeCompletionContract(true, true, true),
                                listOf(
                                    NativeCompletionStepStatus("commit", "succeeded", "ok"),
                                    NativeCompletionStepStatus("push", "unknown", "not certified")
                                )
                            )
                        )
                    ).completion?.finishActionPending
                )
            } finally {
                panel.shutdown()
            }

            // T19: the conflict surfaces keep the pinned `! Conflict` fact
            // and render the exact label+copy alongside it; the action has no
            // control, so it never renders.
            val task = parseNativeTaskViews(TASK_JSON)[0]
            val conflictedProof = parseNativeTaskProof(
                TASK_PROOF_JSON.replace("\"conflictCount\":0", "\"conflictCount\":1")
            )
            val conflicted = TaskTree.build(task = task, proof = conflictedProof)
            val conflictFacts = conflicted.proof?.integrationFacts()
                ?: fail("conflicted proof view")
            assertTrue(conflictFacts.contains("! Conflict"), conflictFacts.toString())
            assertTrue(
                conflictFacts.contains(RunStateCopy.WORKSPACE_CHANGED_LABEL),
                conflictFacts.toString()
            )
            assertTrue(
                conflictFacts.contains(RunStateCopy.WORKSPACE_CHANGED_COPY),
                conflictFacts.toString()
            )
            assertTrue(
                !conflictFacts.contains(RunStateCopy.WORKSPACE_CHANGED_ACTION),
                "the T19 action has no affordance: $conflictFacts"
            )
            val overview = OverviewPanel()
            overview.updateModel(conflicted)
            val integrationRow = overview.summaryText("Integration")
            assertTrue(integrationRow.contains("! Conflict"), integrationRow)
            assertTrue(
                integrationRow.contains(RunStateCopy.WORKSPACE_CHANGED_LABEL),
                integrationRow
            )
            assertTrue(
                integrationRow.contains(RunStateCopy.WORKSPACE_CHANGED_COPY),
                integrationRow
            )
            // Negative: a clean served integration never renders the copy.
            val clean = TaskTree.build(task = task, proof = parseNativeTaskProof(TASK_PROOF_JSON))
            assertTrue(
                !clean.proof!!.integrationFacts().contains(RunStateCopy.WORKSPACE_CHANGED_LABEL),
                clean.proof!!.integrationFacts().toString()
            )
        }

        step("history unavailable renders the T31 copy with its refresh action") {
            val panel = HistoryPanel()
            var refreshes = 0
            panel.setListener(object : HistoryPanel.Listener {
                override fun onOpenSession(sessionId: String) {}

                override fun onRefresh() {
                    refreshes++
                }
            })
            panel.setUnavailable("history read refused (status 503)")
            assertEquals(false, panel.available())
            assertEquals("History unavailable", panel.unavailableHeadlineText())
            assertEquals("The conversation list could not be read.", panel.unavailableCopyText())
            assertEquals("Refresh history", panel.refreshHistoryActionText())
            panel.refreshHistoryButtonForTest().doClick()
            assertEquals(1, refreshes)
            panel.update(parseNativeSessionList(SMOKE_SESSIONS_JSON), null)
            assertEquals(true, panel.available())
        }

        step("attachments ride the task-run/tournament requests") {
            assertEquals(
                "{\"goal\":\"g\",\"criteria\":[\"c\"],\"files\":[\"/tmp/a.rs\",\"/tmp/b.rs\"]}",
                NativeRequests.startTaskRun("g", listOf("c"), files = listOf("/tmp/a.rs", "/tmp/b.rs"))
            )
            assertEquals(
                "{\"goal\":\"g\"}",
                NativeRequests.startTaskRun("g")
            )
            // The logical-start idempotency identity rides the strict body
            // member; absent id keeps the default path byte-identical.
            assertEquals(
                "{\"goal\":\"g\",\"submission_id\":\"sub-1\"}",
                NativeRequests.startTaskRun("g", submissionId = "sub-1")
            )
            // Image parity: a durable attachment id rides the SAME task-run
            // DTO member in the same representation the VS Code client uses.
            val attachmentId = NativeAttachmentId(
                "a".repeat(64), "image/png", "shot.png", 4
            )
            assertEquals(
                "{\"goal\":\"g\",\"attachments\":[{\"digest\":\"" + "a".repeat(64) +
                    "\",\"mime\":\"image/png\",\"filename\":\"shot.png\",\"size\":4}]}",
                NativeRequests.startTaskRun("g", attachments = listOf(attachmentId))
            )
            assertEquals(
                "{\"mime\":\"image/png\",\"filename\":\"shot.png\",\"data_base64\":\"iVBORw==\"}",
                NativeRequests.uploadAttachment("image/png", "shot.png", "iVBORw==")
            )
            val parsedId = parseNativeAttachmentId(
                "{\"digest\":\"" + "b".repeat(64) +
                    "\",\"mime\":\"image/png\",\"filename\":null,\"size\":4}"
            )
            assertEquals("image/png", parsedId.mime)
            assertEquals(4L, parsedId.size)
            // The upload response is the FULL reference (additive `ref_id`):
            // the generated ProtocolAttachmentRef parser keeps it and the
            // task-admission projection drops only `ref_id`.
            val parsedRef = parseProtocolAttachmentRef(
                JsonCodec.parse(
                    "{\"ref_id\":3,\"digest\":\"" + "b".repeat(64) +
                        "\",\"mime\":\"image/png\",\"filename\":null,\"size\":4}"
                ).view("attachment ref")
            )
            assertEquals(3L, parsedRef.refId)
            assertEquals("b".repeat(64), parsedRef.digest)
            assertEquals(
                NativeAttachmentId("b".repeat(64), "image/png", null, 4),
                parsedRef.attachmentId(),
                "the task-admission projection keeps digest/mime/filename/size only"
            )
            assertEquals(
                "{\"goal\":\"g\",\"criteria\":[\"c\"],\"n\":3,\"files\":[\"/tmp/a.rs\"]}",
                NativeRequests.startTournament("g", listOf("c"), 3, files = listOf("/tmp/a.rs"))
            )
            // The strict reply DTO carries the OWNING session id (the fixture
            // permission belongs to session 9, not the panel's session 7).
            assertEquals(
                "{\"session_id\":\"9\",\"permission_id\":\"7\",\"decision\":\"allow\"}",
                NativeRequests.permissionReply("9", "7", "allow")
            )
            assertEquals(
                "{\"selector\":\"line_range\",\"start\":2,\"end\":5}",
                NativeRequests.evidenceSelectorLines(2, 5)
            )
        }

        step("engineering contract decodes additively and rides task starts") {
            // A legacy daemon (no member) stays an honest null, never a
            // fabricated empty contract.
            val legacy = parseNativeTaskViews(TASK_JSON).single()
            assertEquals(null, legacy.contract)
            assertEquals(null, legacy.contractMalformed)
            assertEquals(0, legacy.contractDroppedRows)
            val parsed = parseNativeTaskViews(CONTRACT_TASK_JSON).single()
            val contract = parsed.contract ?: fail("the served contract must decode")
            assertEquals(1, contract.constraints.size)
            assertEquals("existing sessions remain valid", contract.constraints[0].statement)
            assertEquals("user", contract.constraints[0].origin.label)
            assertEquals("example.graph", contract.invariants[0].origin.label)
            assertEquals("high", contract.risks[0].severity)
            assertEquals("test execution", contract.proofObligations[0].evidenceLabel)
            assertEquals("supported · 91%", contract.proofObligations[0].stateText)
            assertTrue(contract.needsDisclosure)
            // The panel projection carries the exact disclosure lines.
            val projected = TaskTree.build(task = parsed).contract
                ?: fail("the projection must keep the served contract")
            assertEquals(
                listOf("risk:high · race conditions", "inferred · replayed refresh tokens fail"),
                projected.disclosureLines()
            )
            // Per-entry resilience: malformed rows are counted, a non-object
            // entry is dropped, and a bad severity/state lands on the WEAKEST
            // state (unknown => disclosure), never a lesser claim.
            val malformed = parseNativeTaskViews(CONTRACT_MALFORMED_JSON).single()
            assertEquals(2, malformed.contractDroppedRows)
            assertEquals("unknown", malformed.contract!!.risks[0].severity)
            assertEquals("unknown", malformed.contract!!.proofObligations[0].state)
            assertEquals(
                "not_a_kind",
                malformed.contract!!.proofObligations[0].evidenceLabel
            )
            assertEquals(null, malformed.contract!!.proofObligations[0].confidenceBps)
            // A present non-object member is an explicit malformed read.
            val unreadable = parseNativeTaskViews(CONTRACT_UNREADABLE_JSON).single()
            assertEquals(null, unreadable.contract)
            assertEquals("the contract member is not an object", unreadable.contractMalformed)
            // The wire rows: only non-empty category arrays serialize, and an
            // all-empty instance stays the byte-identical default path.
            assertEquals(
                "{\"goal\":\"g\",\"constraints\":[\"keep\"]}",
                NativeRequests.startTaskRun(
                    "g",
                    contractRows = NativeContractRows(constraints = listOf("keep"))
                )
            )
            assertEquals(
                "{\"goal\":\"g\",\"proof_obligations\":[\"claim\"]}",
                NativeRequests.startTaskRun(
                    "g",
                    contractRows = NativeContractRows(proofObligations = listOf("claim"))
                )
            )
            assertEquals(
                "{\"goal\":\"g\"}",
                NativeRequests.startTaskRun("g", contractRows = NativeContractRows())
            )
        }

        // P1-JETBRAINS: lexical containment is not object identity. An
        // in-workspace symlink that resolves outside must be refused by the
        // authoritative (real-path revalidated) attachment read, while a real
        // in-workspace file still reads.
        step("attachment reads are canonicalized and symlink escapes refuse") {
            val dir = Files.createTempDirectory("faktor-jb-attach-")
            val root = Files.createDirectories(dir.resolve("ws"))
            val outside = Files.createDirectories(dir.resolve("outside"))
            val secret = Files.write(outside.resolve("secret.txt"), "top-secret".toByteArray())
            Files.write(root.resolve("inside.txt"), "ok".toByteArray())
            Files.createSymbolicLink(root.resolve("escape.txt"), secret)
            val refusal = refusalOf {
                readAttachmentBounded("escape.txt", root, 1024)
            }
            assertEquals("outside_workspace", refusal.code)
            val inside = readAttachmentBounded("inside.txt", root, 1024)
            assertEquals("ok", String(inside!!, Charsets.UTF_8))
            // An absolute path outside the root is refused at the lexical
            // stage, and a symlinked parent directory cannot smuggle it in.
            val linkedDir = Files.createSymbolicLink(dir.resolve("linked-outside"), outside)
            val viaDir = linkedDir.resolve("secret.txt").toString()
            val viaRefusal = refusalOf {
                readAttachmentBounded(viaDir, root, 1024)
            }
            assertEquals("outside_workspace", viaRefusal.code)
        }

        // Audit findings 2/4: the attachment byte RESPONSE bound mirrors the
        // daemon's HTTP upload/retrieval contract (7 MiB), never the generic
        // 4 MiB body cap; and the blob metadata route is surfaced with its
        // 404/1/409 semantics instead of staying server-only.
        step("attachment retrieval: 7 MiB HTTP contract bound; blob metadata 404/1/409") {
            val server = com.sun.net.httpserver.HttpServer.create(
                java.net.InetSocketAddress("127.0.0.1", 0), 0
            )
            val mib = 1024 * 1024
            // Generated bounded payloads (no embedded literals): 4 MiB + 1
            // and exactly 7 MiB fit the retrieval contract; 7 MiB + 1 does not.
            val justOverFour = ByteArray(4 * mib + 1)
            val seven = ByteArray(7 * mib)
            val overSeven = ByteArray(7 * mib + 1)
            for (buffer in listOf(justOverFour, seven, overSeven)) {
                buffer[0] = 0x5a
                buffer[buffer.size - 1] = 0xa5.toByte()
            }
            val digestBlobFour = "a".repeat(64)
            val digestRefSeven = "b".repeat(64)
            val digestOver = "c".repeat(64)
            val digestSolo = "d".repeat(64)
            val digestMissing = "e".repeat(64)
            val digestShared = "f".repeat(64)
            server.createContext("/native/session/7/attachments/ref/1/bytes") { exchange ->
                respondAttachmentBytes(exchange, 200, "application/pdf", justOverFour)
            }
            server.createContext("/native/session/7/attachments/ref/2/bytes") { exchange ->
                respondAttachmentBytes(exchange, 200, "application/pdf", seven)
            }
            server.createContext("/native/session/7/attachments/ref/3/bytes") { exchange ->
                respondAttachmentBytes(exchange, 200, "application/pdf", overSeven)
            }
            server.createContext("/native/session/7/attachments/blob/$digestBlobFour/bytes") { exchange ->
                respondAttachmentBytes(exchange, 200, "application/octet-stream", justOverFour)
            }
            server.createContext("/native/session/7/attachments/blob/$digestRefSeven/bytes") { exchange ->
                respondAttachmentBytes(exchange, 200, "application/octet-stream", seven)
            }
            server.createContext("/native/session/7/attachments/blob/$digestOver/bytes") { exchange ->
                respondAttachmentBytes(exchange, 200, "application/octet-stream", overSeven)
            }
            // Blob metadata: exactly one reference resolves, zero is a typed
            // 404, several are the typed 409 conflict listing both ref ids.
            server.createContext("/native/session/7/attachments/blob/$digestSolo") { exchange ->
                respondAttachmentJson(
                    exchange, 200,
                    "{\"ref_id\":11,\"digest\":\"$digestSolo\",\"mime\":\"application/pdf\"," +
                        "\"filename\":\"spec.pdf\",\"size\":8}"
                )
            }
            server.createContext("/native/session/7/attachments/blob/$digestMissing") { exchange ->
                respondAttachmentJson(
                    exchange, 404,
                    "{\"error\":{\"code\":\"not_found\",\"message\":\"attachment blob " +
                        "$digestMissing in session 7\",\"retryable\":false}}"
                )
            }
            server.createContext("/native/session/7/attachments/blob/$digestShared") { exchange ->
                respondAttachmentJson(
                    exchange, 409,
                    "{\"error\":{\"code\":\"conflict\",\"message\":\"attachment digest " +
                        "$digestShared in session 7 has 2 references; resolve one by ref_id: " +
                        "[11, 12]\",\"retryable\":false}}"
                )
            }
            server.start()
            val client = NativeClient(
                "http://127.0.0.1:" + server.address.port, "smoke-password"
            )
            try {
                assertEquals(
                    4L * 1024 * 1024, NativeClient.DEFAULT_MAX_BODY_BYTES,
                    "the generic body cap stays 4 MiB"
                )
                assertEquals(
                    7L * 1024 * 1024, NativeClient.ATTACHMENT_RESPONSE_MAX_BYTES,
                    "the attachment response bound mirrors the daemon HTTP contract"
                )
                val refFour = client.attachmentReferenceBytes("7", 1L)
                assertEquals(
                    4 * mib + 1, refFour.bytes.size,
                    "a 4 MiB + 1 ref payload exceeds the old 4 MiB cap and still retrieves"
                )
                assertEquals(0x5a.toByte(), refFour.bytes[0], "first byte byte-exact")
                assertEquals(
                    0xa5.toByte(), refFour.bytes[refFour.bytes.size - 1], "last byte byte-exact"
                )
                assertEquals(
                    7 * mib, client.attachmentReferenceBytes("7", 2L).bytes.size,
                    "exactly 7 MiB ref bytes retrieve byte-exact"
                )
                assertEquals(
                    4 * mib + 1, client.attachmentBlobBytes("7", digestBlobFour).size,
                    "a 4 MiB + 1 blob payload exceeds the old 4 MiB cap and still retrieves"
                )
                assertEquals(
                    7 * mib, client.attachmentBlobBytes("7", digestRefSeven).size,
                    "exactly 7 MiB blob bytes retrieve byte-exact"
                )
                val refOver: () -> Unit = { client.attachmentReferenceBytes("7", 3L) }
                val blobOver: () -> Unit = { client.attachmentBlobBytes("7", digestOver) }
                for ((label, call) in listOf("ref" to refOver, "blob" to blobOver)) {
                    try {
                        call()
                        fail("$label 7 MiB + 1 must be refused typed")
                    } catch (e: NativeProtocolException) {
                        assertTrue(
                            (e.message ?: "").contains("exceeded bound"),
                            "$label: ${e.message}"
                        )
                    }
                }
                val solo = client.attachmentBlobReference("7", digestSolo)
                assertEquals(11L, solo.refId, "one reference resolves its own metadata")
                assertEquals(digestSolo, solo.digest)
                try {
                    client.attachmentBlobReference("7", digestMissing)
                    fail("zero references must not answer 200")
                } catch (e: NativeApiException) {
                    assertEquals(404, e.status, "zero references: ${e.message}")
                }
                try {
                    client.attachmentBlobReference("7", digestShared)
                    fail("several references must not answer 200")
                } catch (e: NativeApiException) {
                    assertEquals(409, e.status, "several references: ${e.message}")
                    assertEquals("conflict", e.code, e.message)
                    assertTrue(
                        (e.message ?: "").contains("11") && (e.message ?: "").contains("12"),
                        "the 409 must list both candidate ref ids: ${e.message}"
                    )
                }
            } finally {
                server.stop(0)
            }
        }

        step("completion contract: strict request, strict parse, durable provenance") {
            // The default path stays byte-identical (no contract, no item).
            assertEquals("{\"goal\":\"g\"}", NativeRequests.startTaskRun("g"))
            assertEquals(
                "{\"goal\":\"g\"}",
                NativeRequests.startTaskRun(
                    "g",
                    completionContract = NativeCompletionContract(false, false, false)
                )
            )
            // A non-default contract starts ONE explicit mutating work item.
            assertEquals(
                "{\"goal\":\"g\",\"criteria\":[\"c\"]," +
                    "\"work_items\":[{\"id\":\"main\",\"kind\":\"Implementation\"," +
                    "\"summary\":\"g\",\"ownership\":\"isolated_worktree\"}]," +
                    "\"completion_contract\":{\"include_commit\":true,\"include_push\":false," +
                    "\"include_pr\":true}}",
                NativeRequests.startTaskRun(
                    "g",
                    listOf("c"),
                    completionContract = NativeCompletionContract(true, false, true)
                )
            )
            // The additive durable completion block parses strictly.
            val served = parseNativeTaskViews(
                "[{\"goal\":\"g\",\"state\":\"running\"," +
                    "\"milestones\":{\"completed\":[],\"open\":[]}," +
                    "\"changedFiles\":[],\"tests\":{\"run\":[],\"failed\":[]},\"budget\":null," +
                    "\"completion\":{\"contract\":{\"include_commit\":true," +
                    "\"include_push\":true,\"include_pr\":false}," +
                    "\"steps\":[{\"step\":\"commit\",\"status\":\"succeeded\"," +
                    "\"detail\":\"abc\"}]}}]"
            )[0]
            val servedCompletion = served.completion ?: fail("served completion must parse")
            assertEquals(true, servedCompletion.contract.includeCommit)
            assertEquals(1, servedCompletion.steps.size)
            assertEquals("succeeded", servedCompletion.steps[0].status)
            assertEquals("abc", servedCompletion.steps[0].detail)
            // A malformed served block is a loud protocol failure.
            try {
                parseNativeTaskViews(
                    "[{\"goal\":\"g\",\"state\":\"running\"," +
                        "\"milestones\":{\"completed\":[],\"open\":[]}," +
                        "\"changedFiles\":[],\"tests\":{\"run\":[],\"failed\":[]},\"budget\":null," +
                        "\"completion\":{\"contract\":{\"include_commit\":true},\"steps\":[]}}]"
                )
                fail("a malformed completion block must fail loudly")
            } catch (e: NativeProtocolException) {
                assertTrue(e.message?.contains("include_push") == true, e.message)
            }
            // Without a served read the block derives from the DURABLE run state.
            val task = parseNativeTaskViews(TASK_JSON)[0]
            assertEquals(null, TaskTree.build(task = task).completion, "no contract => no block")
            val pending = TaskTree.build(
                task = task,
                submittedCompletion = NativeCompletionContract(true, false, true),
                runState = "Running"
            ).completion ?: fail("pending completion must build")
            assertEquals("derived", pending.source)
            assertEquals(2, pending.steps.size)
            assertEquals("pending", pending.steps[0].status)
            val certified = TaskTree.build(
                task = task,
                submittedCompletion = NativeCompletionContract(true, false, false),
                runState = "Done"
            ).completion ?: fail("certified completion must build")
            assertEquals("derived", certified.source)
            assertEquals("succeeded", certified.steps[0].status)
            val unknown = TaskTree.build(
                task = task,
                submittedCompletion = NativeCompletionContract(false, true, false),
                runState = "Failed"
            ).completion ?: fail("terminal completion must build")
            assertEquals("unavailable", unknown.source)
            assertEquals("unknown", unknown.steps[0].status)
            assertTrue(
                unknown.reason?.contains("no per-step completion read") == true,
                unknown.reason
            )
            // Daemon-served rows win and the panel renders them as rows.
            val daemon = TaskTree.build(task = served).completion ?: fail("daemon completion")
            assertEquals("daemon", daemon.source)
            assertEquals(listOf("[succeeded] commit - abc"), TaskTreePanel().completionLabels(daemon))
        }

        step("Task-mode contract controls are checked only in the Work composer") {
            val panel = FaktorChatPanel(
                FaktorFrontendService(
                    Paths.get("target/debug/faktor-cli"),
                    Paths.get(System.getProperty("java.io.tmpdir"), "faktor-frontend-contract-smoke")
                )
            )
            try {
                assertEquals(null, panel.completionContractFromControls())
                panel.selectFinishLevelForTest(FinishLevel.PR)
                assertEquals(
                    NativeCompletionContract(includeCommit = true, includePush = true, includePr = true),
                    panel.completionContractFromControls()
                )
                panel.selectFinishLevelForTest(FinishLevel.LOCAL)
                assertEquals(null, panel.completionContractFromControls())
            } finally {
                panel.shutdown()
            }
        }

        step("canned native parsing: durable board page + post + request body") {
            val page = parseNativeBoardPage(BOARD_PAGE_JSON)
            assertEquals(7L, page.boardId)
            assertEquals(3L, page.revision)
            assertEquals(2, page.posts.size)
            assertEquals("handoff", page.posts[0].subject)
            assertEquals(8L, page.posts[0].authorChild)
            assertEquals(null, page.posts[1].authorChild)
            assertEquals(listOf("evidence:41"), page.posts[0].refs)
            assertEquals(2L, page.nextBeforeRevision)
            assertEquals(true, page.hasMore)
            assertEquals(3L, parseNativeBoardPost(BOARD_POST_JSON).revision)
            assertEquals(
                "{\"subject\":\"s\",\"body\":\"b\",\"refs\":[\"evidence:41\"]}",
                NativeRequests.boardPost("s", "b", listOf("evidence:41"))
            )
            assertEquals("{\"subject\":\"s\",\"body\":\"b\"}", NativeRequests.boardPost("s", "b"))
            // A malformed page is a loud protocol failure, never an empty board.
            try {
                parseNativeBoardPage(
                    "{\"board_id\":7,\"revision\":3,\"posts\":[]," +
                        "\"next_before_revision\":null}"
                )
                fail("a board page without has_more must fail loudly")
            } catch (e: NativeProtocolException) {
                assertTrue(e.message?.contains("has_more") == true, e.message)
            }
        }

        step("board panel records availability truthfully and gates the composer") {
            val panel = BoardPanel()
            panel.setUnavailable("no board route (status 404 not_found: no route)")
            assertEquals(false, panel.available())
            assertEquals(false, panel.postEnabled())
            assertEquals(false, panel.readEnabled())
            assertTrue(panel.headerText().contains("no board route"), panel.headerText())

            var reads = 0
            var postedSubject: String? = null
            var postedBody: String? = null
            panel.setListener(object : BoardPanel.Listener {
                override fun onRead() { reads++ }
                override fun onPost(subject: String, body: String) {
                    postedSubject = subject
                    postedBody = body
                }
            })
            panel.setComposerFields("status", "all green")
            panel.submitComposer()
            assertEquals(null, postedSubject, "an unavailable panel must not post")
            assertEquals(0, reads, "an unavailable panel must not read")

            panel.setBoard(parseNativeBoardPage(BOARD_PAGE_JSON))
            assertEquals(true, panel.available())
            assertEquals(true, panel.postEnabled())
            assertEquals(true, panel.readEnabled())
            assertTrue(panel.headerText().contains("2 unread"), panel.headerText())
            assertTrue(panel.headerText().contains("2 posts"), panel.headerText())
            assertTrue(panel.postsText().contains("Agent 8"), panel.postsText())
            assertTrue(panel.postsText().contains("handoff"), panel.postsText())
            assertTrue(panel.postsText().contains("Conversation"), panel.postsText())
            assertTrue(panel.postsText().contains("root note"), panel.postsText())
            assertTrue(panel.advancedText().contains("revision 3 [child:8]"), panel.advancedText())
            panel.submitComposer()
            assertEquals("status", postedSubject)
            assertEquals("all green", postedBody)
            // An explicit read acknowledges the page; an automatic refresh
            // never moves the watermark.
            panel.setBoard(parseNativeBoardPage(BOARD_PAGE_JSON), acknowledge = true)
            assertTrue(panel.headerText().contains("0 unread"), panel.headerText())
            // Byte-bound parity with the daemon and the VS Code panel: 512
            // emoji is 2048 UTF-8 bytes and must be refused LOCALLY (never
            // accepted and silently truncated), while 340 bytes is inside.
            panel.setComposerFields("\uD83D\uDE00".repeat(512), "body")
            assertEquals(false, panel.postEnabled())
            assertTrue(
                panel.draftRefusal()?.contains("512") == true,
                panel.draftRefusal()
            )
            panel.setComposerFields("s\u00e9".repeat(170), "body")
            assertEquals(true, panel.postEnabled(), "340 bytes is inside the 512-byte bound")

            panel.reset()
            assertEquals(false, panel.available())
            assertEquals(false, panel.postEnabled())
        }

        step("display bounds strip invisible controls and never split surrogate pairs") {
            // A cut between a high and a low surrogate backs off one unit
            // instead of rendering U+FFFD: "a" + emoji + "b" bounded to 2.
            assertEquals("a...", bound("a\uD83D\uDE00b", 2))
            // At the exact boundary the pair is kept whole.
            assertEquals("a\uD83D\uDE00", bound("a\uD83D\uDE00", 3))
            // C0 controls and bidi overrides never reach a visible label.
            assertEquals("abc", bound("a\u0000b\u200Ec", 10))
            assertEquals("abc", bound("a\u2066b\u2069c", 10))
            // Newlines and tabs are legitimate line structure and survive.
            assertEquals("a\nb\tc", bound("a\nb\tc", 10))
        }

        step("evidence ref parsing mirrors the cockpit vocabulary") {
            assertEquals(41L, EvidenceRefs.parse("evidence:41").id)
            assertEquals(42L, EvidenceRefs.parse("see evidence/42 here").id)
            assertEquals(43L, EvidenceRefs.parse("evidence#43").id)
            assertEquals(null, EvidenceRefs.parse("free text").id)
        }

        step("pixel identity is deterministic and every state animates") {
            val a = PixelAgents.avatar("child-1")
            val b = PixelAgents.avatar("child-1")
            assertEquals(a, b)
            assertEquals(25, a.pixels.size)
            for (y in 0 until 5) {
                for (x in 0 until 3) {
                    assertEquals(
                        a.pixels[y * 5 + x],
                        a.pixels[y * 5 + (4 - x)],
                        "sprite must be symmetric"
                    )
                }
            }
            assertTrue(a.hue in 0..359, "hue must be in range")
            val states = mapOf(
                "Running" to PixelState.RUNNING,
                "Paused" to PixelState.PAUSED,
                "Waiting" to PixelState.WAITING,
                "Blocked" to PixelState.BLOCKED,
                "Done" to PixelState.DONE,
                "FailedRecoverable" to PixelState.FAILED,
                "Cancelled" to PixelState.CANCELLED,
                "mystery" to PixelState.WAITING
            )
            for ((tag, expected) in states) {
                assertEquals(expected, PixelAgents.stateOf(tag), "state $tag")
            }
            assertEquals("pixel-blocked", PixelState.BLOCKED.animation)
            assertTrue(
                PixelAgents.frame(PixelState.RUNNING, 0) != PixelAgents.frame(PixelState.RUNNING, 2),
                "running must bob"
            )
            assertEquals(true, PixelAgents.frame(PixelState.BLOCKED, 0).alert)
            assertTrue(
                PixelAgents.frame(PixelState.FAILED, 0).alpha != PixelAgents.frame(PixelState.FAILED, 1).alpha,
                "failed must flicker during its bounded transition"
            )
            // Terminal states end static: the bounded transition is over at
            // TERMINAL_TRANSITION_TICKS and the frame never changes again.
            assertEquals(false, PixelAgents.isStatic(PixelState.DONE, 0), "done may transition briefly")
            assertEquals(
                true,
                PixelAgents.isStatic(PixelState.DONE, PixelAgents.TERMINAL_TRANSITION_TICKS),
                "done must settle static"
            )
            val settledDone = PixelAgents.frame(PixelState.DONE, PixelAgents.TERMINAL_TRANSITION_TICKS)
            assertEquals(
                settledDone,
                PixelAgents.frame(PixelState.DONE, PixelAgents.TERMINAL_TRANSITION_TICKS + 1),
                "done frame index must be stable across ticks"
            )
            assertEquals(
                settledDone,
                PixelAgents.frame(PixelState.DONE, 1000),
                "done frame index must be stable forever"
            )
            val settledFailed = PixelAgents.frame(PixelState.FAILED, PixelAgents.TERMINAL_TRANSITION_TICKS)
            assertEquals(
                settledFailed,
                PixelAgents.frame(PixelState.FAILED, 1000),
                "failed frame index must be stable after its transition"
            )
            assertEquals(
                true,
                PixelAgents.isStatic(PixelState.CANCELLED, 0),
                "cancelled is static (slash) from its first frame"
            )
            // The owning sprite timer stops at the settle boundary: DONE
            // runs a bounded transition and then stops animating.
            val sprite = PixelSprite("child-1", "Running")
            sprite.setState("Done")
            var guard = 0
            while (sprite.animationRunning() && guard < 10) {
                sprite.advance()
                guard++
            }
            assertEquals(false, sprite.animationRunning(), "Done must stop the sprite timer")
            assertEquals(true, sprite.settled(), "Done must end on a settled frame")
            assertEquals(
                PixelAgents.TERMINAL_TRANSITION_TICKS,
                sprite.tick(),
                "the terminal transition is bounded"
            )
            // Reduced motion (platform setting, or explicit override): no
            // timer ever starts and terminal states stay static.
            PixelMotion.override = true
            try {
                val reduced = PixelSprite("child-1", "Running")
                reduced.syncTimer()
                assertEquals(false, reduced.animationRunning(), "reduced motion must not animate")
                val reducedDone = PixelSprite("child-1", "Waiting")
                reducedDone.setState("Done")
                assertEquals(
                    false,
                    reducedDone.animationRunning(),
                    "reduced motion keeps a Done sprite static"
                )
            } finally {
                PixelMotion.override = null
            }
            assertEquals(
                PixelState.CANCELLED,
                PixelAgents.fold(
                    mapOf("child-1" to PixelAgents.presence("child-1", "Done")),
                    listOf(Pair("child-1", "Cancelled"))
                )["child-1"]?.state
            )
            val retained = PixelAgents.fold(
                mapOf("child-1" to PixelAgents.presence("child-1", "Done")),
                emptyList()
            )
            assertEquals(1, retained.size)
            assertEquals(PixelState.DONE, retained["child-1"]?.state)
            val image = BufferedImage(22, 22, BufferedImage.TYPE_INT_ARGB)
            val graphics = image.createGraphics()
            try {
                PixelSprite.paintSprite(graphics, PixelAgents.presence("child-1", "Blocked"), 1)
            } finally {
                graphics.dispose()
            }
        }

        step("task tree model surfaces every section from mock payloads") {
            val task = parseNativeTaskViews(TASK_JSON)[0]
            val agents = parseNativeAgents(AGENTS_JSON)
            val catalog = parseNativeModelCatalog(MODELS_JSON)
            val usage = parseNativeSessionUsage(SESSION_USAGE_JSON)
            val model = TaskTree.build(
                task = task,
                agents = agents,
                graph = parseNativeOrchestratorGraph(GRAPH_JSON),
                catalog = catalog,
                verification = parseNativeVerificationView(VERIFICATION_JSON),
                taskVerification = parseNativeTaskVerification(TASK_VERIFICATION_JSON),
                usage = usage,
                childUsage = mapOf("9" to usage),
                tournament = parseNativeTournament(TOURNAMENT_JSON),
                proof = parseNativeTaskProof(TASK_PROOF_JSON)
            )
            assertEquals("ship it", model.goal)
            assertEquals("in_progress", model.state)
            assertEquals("building", model.phase)
            assertEquals(listOf("criterion A"), model.acceptanceCriteria)
            assertEquals(2, model.steps.size)
            assertEquals(listOf("step-1"), model.steps[1].dependsOn)
            assertEquals(1, model.children.size)
            val child = model.children[0]
            assertEquals("permission", child.blocker?.kind)
            assertEquals("background", child.presentation)
            assertEquals(true, child.background)
            assertEquals("fake", child.provider)
            assertEquals(true, child.reasoning)
            assertEquals(1000L, child.budgetMaxTokens)
            assertEquals(7L, child.spentTokens)
            assertEquals(993L, child.remainingTokens)
            assertEquals(true, child.progress?.stalled)
            assertEquals("main step output", child.result?.summary)
            assertEquals(1, model.blockers.size)
            assertTrue(
                model.blockers[0].actions.contains(BlockerAction.PERMISSION_ALLOW) &&
                    model.blockers[0].actions.contains(BlockerAction.RESUME) &&
                    model.blockers[0].actions.contains(BlockerAction.RETRY),
                "permission blockers must offer resume/allow/retry"
            )
            assertEquals("passed", model.verification.status)
            assertEquals(1, model.verification.owed)
            // The top-level summary renders VERIFIED with the served
            // criteria/checks/review/tree/commit/remote-head facts.
            assertEquals(true, model.verification.verified)
            val summaryText = model.verification.summaryText()
            assertTrue(summaryText.startsWith("VERIFIED"), summaryText)
            assertTrue(summaryText.contains("criteria 1/1"), summaryText)
            assertTrue(summaryText.contains("checks failed 0"), summaryText)
            assertTrue(summaryText.contains("review review-bot"), summaryText)
            assertTrue(summaryText.contains("tree ver1234"), summaryText)
            assertTrue(summaryText.contains("commit abcdef12"), summaryText)
            assertTrue(summaryText.contains("head refs/9"), summaryText)
            val evidenceIds = model.evidence.mapNotNull { it.id }.sorted()
            assertEquals(listOf(41L, 42L, 43L), evidenceIds)
            assertEquals(93L, model.spend?.remainingTokens)
            assertEquals(true, model.spend?.durable)
            // Background children are tucked after foreground children.
            val foregroundChild = agents[1].copy(agentId = "child-2", presentation = "foreground")
            val ordered = TaskTree.build(agents = listOf(agents[1], foregroundChild)).children
            assertEquals(listOf("child-2", "child-1"), ordered.map { it.childId })
            assertEquals("child-0", model.tournament?.winner)
            assertEquals(true, model.tournament?.candidates?.get(0)?.winner)
            assertEquals(false, model.tournament?.candidates?.get(1)?.winner)
        }

        step("acceptance-criterion proof rows render every wire-served fact") {
            val task = parseNativeTaskViews(TASK_JSON)[0]
            val verification = parseNativeTaskVerification(CRITERION_PROOF_JSON)
            val record = verification.records[0]
            // The record-level P0 proof payload is carried and parsed.
            assertEquals(11L, record.startedMs)
            assertEquals(22L, record.completedMs)
            assertEquals(3L, record.sourceCount)
            assertEquals("cand-snap", record.candidateProof?.candidateSnapshot)
            assertEquals("run-proof", record.candidateProof?.runId)
            assertEquals("verified-snap", record.verifiedSnapshot)
            assertEquals("base-snap", record.basedOnSnapshot)
            assertEquals("landed-snap", record.landedSnapshot)
            assertEquals("abcdef12", record.candidateProof?.publishedCommit)
            assertEquals("refs/9", record.candidateProof?.remotePrHead)
            assertEquals("review-bot", record.reviewer)

            val model = TaskTree.build(task = task, taskVerification = verification)
            val byKey = model.criteriaProof.associateBy { it.criterionKey }

            // All seven binding kinds render straight from the wire with the
            // exact reference, served origin/requirement and three-way
            // verdict — and every string-kind row carries NO fallback marker.
            val served: List<Triple<String, String, String?>> = listOf(
                Triple("check criterion", "required_check", "check:rust_check:digest-check"),
                Triple("coverage criterion", "integration_coverage", "work-item:impl-a, work-item:impl-b"),
                Triple("file criterion", "file_state", "src/a.rs"),
                Triple("evidence criterion", "evidence", "evidence:41"),
                Triple("review criterion", "independent_review", "reviewer-1"),
                Triple("aggregate criterion", "aggregate_goal", null)
            )
            for ((key, kind, reference) in served) {
                val row = byKey[key] ?: fail("$key must render a proof row")
                assertEquals(kind, row.bindingKind, key)
                assertEquals("daemon", row.bindingSource, key)
                assertEquals(reference, row.bindingReference, key)
                assertEquals("pass", row.verdict, key)
                assertEquals(CriterionVerdictTone.PASS, row.tone, key)
                assertTrue(!row.requirement.contains("unavailable"), "$key ${row.requirement}")
                assertTrue(!row.origin.contains("unavailable"), "$key ${row.origin}")
                assertTrue(!row.snapshot.contains("unavailable"), "$key ${row.snapshot}")
                assertTrue(
                    !row.verificationTimestamp.contains("unavailable"),
                    "$key ${row.verificationTimestamp}"
                )
                assertTrue(row.unavailable.isEmpty(), "$key ${row.unavailable}")
            }
            assertEquals("required", byKey["check criterion"]?.requirement)
            assertEquals("user", byKey["check criterion"]?.origin)
            // The served published commit / remote PR head / reviewer render
            // in the snapshot fact line too.
            assertTrue(
                byKey["check criterion"]?.snapshot?.contains("commit abcdef12") == true,
                byKey["check criterion"]?.snapshot ?: ""
            )
            assertTrue(
                byKey["check criterion"]?.snapshot?.contains("head refs/9") == true,
                byKey["check criterion"]?.snapshot ?: ""
            )
            assertTrue(
                byKey["check criterion"]?.snapshot?.contains("review review-bot") == true,
                byKey["check criterion"]?.snapshot ?: ""
            )
            assertEquals("preferred", byKey["review criterion"]?.requirement)
            assertEquals("project_policy", byKey["review criterion"]?.origin)
            assertEquals("semantic_provider", byKey["unavailable criterion"]?.origin)
            assertEquals(
                "check rust_check · command digest digest-check",
                byKey["check criterion"]?.bindingDetail
            )
            assertEquals("expected digest digest-file", byKey["file criterion"]?.bindingDetail)
            assertEquals("reviewer reviewer-1", byKey["review criterion"]?.bindingDetail)
            // The explicitly unavailable binding kind is wire truth: it
            // renders as unavailable with the served reason, never a pass.
            val unavailableRow = byKey["unavailable criterion"] ?: fail("unavailable criterion must render")
            assertEquals("unavailable", unavailableRow.bindingKind)
            assertEquals("unavailable", unavailableRow.verdict)
            assertEquals(CriterionVerdictTone.UNAVAILABLE, unavailableRow.tone)
            assertEquals("daemon", unavailableRow.bindingSource)
            assertEquals("no objective mechanism", unavailableRow.bindingDetail)
            // T15: the claim has no served verdict, so the exact supporting
            // sentence is rendered (never a pass, never a color-only cue).
            assertEquals("No verdict is served for this claim.", unavailableRow.noVerdictCopy)
            assertTrue(!unavailableRow.snapshot.contains("unavailable"), unavailableRow.snapshot)
            assertTrue(
                !unavailableRow.verificationTimestamp.contains("unavailable"),
                unavailableRow.verificationTimestamp
            )
            // A legacy unbound row (old daemon) still DERIVES its binding
            // from typed evidence refs and stays honestly marked.
            val legacy = byKey["legacy criterion"] ?: fail("legacy criterion must render")
            assertEquals("file_state", legacy.bindingKind)
            assertEquals("derived", legacy.bindingSource)
            assertEquals("file:src/legacy.rs", legacy.bindingReference)
            assertEquals("pass", legacy.verdict)
            assertTrue(legacy.unavailable.isNotEmpty(), legacy.unavailable.toString())
            assertEquals(null, legacy.noVerdictCopy)
            // The explicit task criterion has no record verdict: unavailable,
            // with the precise reason — never a fabricated pass.
            val explicit = byKey["criterion A"] ?: fail("the explicit criterion must render a proof row")
            assertEquals("unavailable", explicit.verdict)
            assertEquals("unavailable", explicit.bindingKind)
            assertEquals(CriterionVerdictTone.UNAVAILABLE, explicit.tone)
            assertEquals("No verdict is served for this claim.", explicit.noVerdictCopy)
            assertTrue(
                explicit.verdictReason?.contains("no durable verification record") == true,
                explicit.verdictReason
            )
            assertTrue(explicit.unavailable.contains("verdict"), explicit.unavailable.toString())
            assertTrue(explicit.unavailable.contains("snapshots"), explicit.unavailable.toString())
            // Record-derived rows carry their durable identity and the served
            // proof snapshots/timestamps.
            assertEquals("r1", byKey["check criterion"]?.recordId)
            assertEquals("passed", byKey["check criterion"]?.recordStatus)
            assertEquals(null, explicit.recordId)
            assertTrue(
                byKey["check criterion"]?.snapshot?.contains("cand cand-snap") == true,
                byKey["check criterion"]?.snapshot
            )
            assertTrue(
                byKey["check criterion"]?.snapshot?.contains("src 3") == true,
                byKey["check criterion"]?.snapshot
            )
            // Timestamps render as ISO-8601 UTC (seconds precision, matching
            // the VS Code cockpit), never as raw epoch milliseconds.
            assertTrue(
                byKey["check criterion"]?.verificationTimestamp
                    ?.contains("started 1970-01-01T00:00:00Z") == true,
                byKey["check criterion"]?.verificationTimestamp
            )
            assertTrue(
                byKey["check criterion"]?.verificationTimestamp
                    ?.contains("completed 1970-01-01T00:00:00Z") == true,
                byKey["check criterion"]?.verificationTimestamp
            )
            assertTrue(
                byKey["check criterion"]?.verificationTimestamp?.contains("11ms") != true,
                "raw epoch milliseconds must never reach the panel"
            )
            // Whole-second instants carry EXACTLY one Z (Instant.toString()
            // omits the fraction and used to get another Z appended).
            assertEquals("1970-01-01T00:00:00Z", TaskTree.utcSecondsForTest(0L))
            assertEquals("1970-01-01T00:00:01Z", TaskTree.utcSecondsForTest(1000L))
            assertEquals(-1, TaskTree.utcSecondsForTest(1000L).indexOf("ZZ"))
            // Panel rendering: every served field survives into the label and
            // the verdict tones are distinct (unavailable is never
            // pass-styled).
            val panel = TaskTreePanel()
            panel.update(model)
            val labels = panel.criterionLabels(model)
            assertEquals(model.criteriaProof.size, labels.size)
            assertTrue(
                labels.any {
                    it.startsWith("[pass] check criterion") &&
                        it.contains("requirement required") &&
                        it.contains("origin user") &&
                        it.contains("binding required_check ref check:rust_check:digest-check")
                },
                labels.toString()
            )
            assertTrue(
                labels.any {
                    it.startsWith("[unavailable] unavailable criterion") &&
                        !it.contains("unavailable:")
                },
                labels.toString()
            )
            assertTrue(
                labels.any { it.startsWith("[unavailable] criterion A") },
                labels.toString()
            )
            // T15 copy pin: every claim with no served verdict renders the
            // exact sentence in BOTH per-claim surfaces (tree label + card).
            assertTrue(
                labels.any { it.contains("No verdict is served for this claim.") },
                labels.toString()
            )
            val verificationPanel = VerificationPanel()
            verificationPanel.updateModel(model)
            assertTrue(
                textsOf(verificationPanel).contains("No verdict is served for this claim."),
                textsOf(verificationPanel).toString()
            )
            assertTrue(
                panel.criterionColor(CriterionVerdictTone.PASS) !=
                    panel.criterionColor(CriterionVerdictTone.FAIL),
                "pass/fail tones must differ"
            )
            assertTrue(
                panel.criterionColor(CriterionVerdictTone.PASS) !=
                    panel.criterionColor(CriterionVerdictTone.UNAVAILABLE),
                "pass/unavailable tones must differ"
            )
            assertTrue(
                panel.criterionColor(CriterionVerdictTone.FAIL) !=
                    panel.criterionColor(CriterionVerdictTone.UNAVAILABLE),
                "fail/unavailable tones must differ"
            )
            // Typed retrieval: the criterion's evidence refs are selectable
            // Evidence nodes (the existing navigator route retrieves id 41).
            val evidence = byKey["evidence criterion"] ?: fail("evidence criterion must render")
            assertEquals(1, evidence.evidenceRefs.size)
            assertEquals(41L, evidence.evidenceRefs[0].id)
            assertEquals("evidence:41", evidence.evidenceRefs[0].label)
        }

        step("a missing verification payload degrades every criterion to unavailable, never pass") {
            val task = parseNativeTaskViews(TASK_JSON)[0]
            val model = TaskTree.build(task = task)
            assertTrue(model.criteriaProof.isNotEmpty(), "explicit criteria still render")
            for (row in model.criteriaProof) {
                assertEquals("unavailable", row.verdict)
                assertEquals(CriterionVerdictTone.UNAVAILABLE, row.tone)
                assertEquals("No verdict is served for this claim.", row.noVerdictCopy)
                assertTrue(row.unavailable.contains("verdict"), row.unavailable.toString())
                assertTrue(row.unavailable.contains("binding"), row.unavailable.toString())
                assertTrue(row.verdictReason?.contains("no durable verification record") == true, row.verdictReason)
            }
        }

        step("verification summary renders VERIFIED only under the strict verified proof") {
            val task = parseNativeTaskViews(TASK_JSON)[0]
            val records = parseNativeTaskVerification(TASK_VERIFICATION_JSON)
            // A passing record alone is NOT verified: only the strict proof
            // state may confirm.
            val noProof = TaskTree.build(task = task, taskVerification = records)
            assertEquals(false, noProof.verification.verified)
            val unverified = TaskTree.build(
                task = task,
                taskVerification = records,
                proof = parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(
                        "\"proofState\":\"verified\"",
                        "\"proofState\":\"unverified\""
                    )
                )
            )
            assertEquals("unverified", unverified.proof?.state)
            assertEquals(false, unverified.verification.verified)
            assertEquals("passed", unverified.verification.status)
            // An unavailable proof keeps its served reason and never
            // re-renders the candidate record as verified.
            val unavailable = TaskTree.build(
                task = task,
                taskVerification = records,
                proof = parseNativeTaskProof(TASK_PROOF_UNAVAILABLE_JSON)
            )
            assertEquals(false, unavailable.verification.verified)
            assertEquals("unavailable", unavailable.verification.status)
            assertTrue(
                unavailable.verification.summaryText().startsWith("VERIFICATION UNAVAILABLE"),
                unavailable.verification.summaryText()
            )
            assertTrue(
                unavailable.verification.summaryText().contains("landed_snapshot mismatch"),
                unavailable.verification.summaryText()
            )
            val refused = TaskTree.build(
                task = task,
                taskVerification = records,
                proofUnavailable = "proof read failed: corrupt_durable_state"
            )
            assertEquals(false, refused.verification.verified)
            assertTrue(
                refused.verification.summaryText().contains("corrupt_durable_state"),
                refused.verification.summaryText()
            )
            // The strict verified proof plus the passing record may confirm.
            val verified = TaskTree.build(
                task = task,
                taskVerification = records,
                proof = parseNativeTaskProof(TASK_PROOF_JSON)
            )
            assertEquals(true, verified.verification.verified)
        }

        step("verification panel keeps every served evidence ref as its own button") {
            val model = TaskTree.build(
                taskVerification = parseNativeTaskVerification(MULTI_EVIDENCE_VERIFICATION_JSON)
            )
            val panel = VerificationPanel()
            panel.updateModel(model)
            assertEquals(1, panel.criterionCount())
            assertEquals(listOf("41", "42", "43", "44"), panel.evidenceButtonIds())
        }

        step("overview keeps proof, spend and completed states honest") {
            val task = parseNativeTaskViews(TASK_JSON)[0]
            val records = parseNativeTaskVerification(TASK_VERIFICATION_JSON)
            val panel = OverviewPanel()
            // A passing record without a strict verified proof is candidate
            // level only: the verified wording never appears.
            panel.updateModel(TaskTree.build(task = task, taskVerification = records))
            val candidateRow = panel.summaryText("Verification")
            assertTrue(candidateRow.startsWith("Candidate checks passed"), candidateRow)
            assertTrue(!candidateRow.contains("Verified"), candidateRow)
            // I04/VAL-LAND-2: with no proof read the integration row stays an
            // explicit missing value, never a positive fact.
            assertEquals(
                "No integration facts are served for this run yet",
                panel.summaryText("Integration")
            )
            // A served but unverified proof keeps the candidate wording and
            // never borrows the verified one.
            panel.updateModel(
                TaskTree.build(
                    task = task,
                    taskVerification = records,
                    proof = parseNativeTaskProof(
                        TASK_PROOF_JSON.replace(
                            "\"proofState\":\"verified\"",
                            "\"proofState\":\"unverified\""
                        )
                    )
                )
            )
            val unverifiedRow = panel.summaryText("Verification")
            assertTrue(unverifiedRow.startsWith("Candidate checks passed"), unverifiedRow)
            assertTrue(!unverifiedRow.contains("Verified"), unverifiedRow)
            // An unavailable proof names itself and its served reason.
            panel.updateModel(
                TaskTree.build(
                    task = task,
                    taskVerification = records,
                    proof = parseNativeTaskProof(TASK_PROOF_UNAVAILABLE_JSON)
                )
            )
            val unavailableRow = panel.summaryText("Verification")
            assertTrue(unavailableRow.startsWith("Verification unavailable"), unavailableRow)
            assertTrue(unavailableRow.contains("landed_snapshot mismatch"), unavailableRow)
            // The integration row carries its own fact: the served mismatch
            // is named, never collapsed into the verification wording.
            assertEquals("✗ Mismatch", panel.summaryText("Integration"))
            // Only the strict verified proof renders the verified wording.
            panel.updateModel(
                TaskTree.build(
                    task = task,
                    taskVerification = records,
                    proof = parseNativeTaskProof(TASK_PROOF_JSON)
                )
            )
            assertTrue(
                panel.summaryText("Verification").startsWith("Verified"),
                panel.summaryText("Verification")
            )
            assertEquals(
                "✓ Landed state matches verified state",
                panel.summaryText("Integration")
            )
            // A completed run without verified proof stays neutral: the bare
            // state token never derives the positive tint.
            val completedTask = task.copy(state = "completed", blockers = emptyList())
            panel.updateModel(TaskTree.build(task = completedTask))
            assertEquals(
                SemanticState.DIM,
                panel.runStateOfForTest(TaskTree.build(task = completedTask))
            )
            assertTrue(panel.stateLabelText().contains("completed"), panel.stateLabelText())
            assertTrue(!panel.stateLabelText().contains("Verified"), panel.stateLabelText())
            // Missing spend is explicitly unavailable; a known zero is not.
            panel.updateModel(TaskTree.build(taskVerification = records))
            assertEquals("Cost unavailable", panel.summaryText("Spend"))
            panel.updateModel(
                TaskTree.build(
                    usage = parseNativeSessionUsage(
                        "{\"sessionId\":\"1\",\"providerCalls\":{\"tokens\":0}," +
                            "\"tasks\":[{\"taskId\":\"3\",\"budget\":{\"spentTokens\":0," +
                            "\"spentCostMicro\":0,\"openReservedMicro\":0}}]}"
                    )
                )
            )
            assertTrue(
                panel.summaryText("Spend").startsWith("\$0.0000"),
                panel.summaryText("Spend")
            )
            assertTrue(
                !panel.summaryText("Spend").contains("unavailable"),
                panel.summaryText("Spend")
            )
        }

        step("task tree spend and child rows keep missing values missing") {
            val panel = TaskTreePanel()
            val child = TaskTree.build(agents = parseNativeAgents(AGENTS_JSON)).children[0]
            assertEquals(null, child.spentTokens)
            assertEquals(null, child.spentCostMicro)
            val childLabel = panel.childLabel(child)
            assertTrue(childLabel.contains("tokens=unavailable/1000"), childLabel)
            assertTrue(childLabel.contains("cost=unavailable/unlimited"), childLabel)
            assertTrue(!childLabel.contains("tokens=0/"), childLabel)
            val spend = panel.spendText(
                SpendSummary(null, null, null, null, BigInteger.ZERO, null, null, true)
            )
            assertTrue(spend.contains("tokens unavailable/unlimited"), spend)
            assertTrue(spend.contains("cost unavailable/unlimited micro"), spend)
            assertTrue(!spend.contains("tokens 0/"), spend)
        }

        step("same model two providers: each child joins its OWN provider metadata") {
            val catalog = parseNativeModelCatalog(DUAL_MODELS_JSON)
            assertEquals(2, catalog.size)
            assertEquals("m", catalog[0].model)
            assertEquals("m", catalog[1].model)
            val child = parseNativeAgents(AGENTS_JSON)[1]
            val alpha = child.copy(agentId = "child-alpha", provider = "alpha")
            val beta = child.copy(agentId = "child-beta", provider = "beta")
            val model = TaskTree.build(agents = listOf(alpha, beta), catalog = catalog)
            assertEquals(2, model.children.size)
            assertEquals("alpha", model.children[0].provider)
            assertEquals(true, model.children[0].reasoning)
            assertEquals(true, model.children[0].tools)
            assertEquals("beta", model.children[1].provider)
            assertEquals(false, model.children[1].reasoning)
            assertEquals(false, model.children[1].tools)
            // A provider-less child never guesses metadata by model alone.
            val bare = TaskTree.build(
                agents = listOf(child.copy(provider = null)),
                catalog = catalog
            ).children[0]
            assertEquals(null, bare.provider)
            assertEquals(null, bare.reasoning)
            assertEquals(null, bare.tools)
        }

        step("strict proof summary renders VERIFIED only from the daemon verdict") {
            val task = parseNativeTaskViews(TASK_JSON)[0]
            val proof = parseNativeTaskProof(TASK_PROOF_JSON)
            assertEquals("faktor-task-proof/v1", proof.schema)
            assertEquals("verified", proof.proofState)
            assertEquals(2, proof.criteriaPassed)
            assertEquals(2, proof.criteriaTotal)
            assertEquals(2, proof.requiredPassed)
            assertEquals("review-bot", proof.reviewer)
            assertEquals(true, proof.landedEqualsVerified)
            assertEquals("pr-42", proof.pullRequestId)
            assertEquals(BigInteger.valueOf(12L), proof.spentCostMicro)
            assertEquals(2, proof.steps.size)
            // The served `integration` block parses additively (I04 /
            // VAL-LAND-2) with the exact VS Code integration DTO shape.
            val integration = proof.integration ?: fail("the served integration block must parse")
            assertEquals(true, integration.present)
            assertEquals(false, integration.inFlight)
            assertEquals("run-1", integration.runId)
            assertEquals("tm1:bbb", integration.finalSnapshotHash)
            assertEquals("tm1:bbb", integration.landedSnapshot)
            assertEquals(1L, integration.integratedFileCount)
            assertEquals(0L, integration.conflictCount)
            assertEquals("blake3:x", integration.txn?.txnId)
            assertEquals("landed", integration.txn?.phase)

            val model = TaskTree.build(task = task, proof = proof)
            val view = model.proof ?: fail("a served proof must build the view")
            assertTrue(view.verified, "the daemon's verified verdict renders verified")
            val summary = view.summaryText()
            assertTrue(summary.startsWith("VERIFIED"), summary)
            assertTrue(summary.contains("criteria 2/2"), summary)
            assertTrue(summary.contains("checks 3/3 (required 2/2)"), summary)
            assertTrue(summary.contains("review passed"), summary)
            assertTrue(summary.contains("verified==landed: yes"), summary)
            assertTrue(summary.contains("commit 22222222"), summary)
            assertTrue(summary.contains("remote head 22222222"), summary)
            assertTrue(summary.contains("PR pr-42"), summary)
            assertTrue(summary.contains("spend 12micro of 1000000micro"), summary)
            // I04 / T18 / VAL-LAND-2: the integration domain renders as its
            // own fact (text + glyph), never folded into the verdict.
            assertEquals(
                listOf("✓ Landed state matches verified state"),
                view.integrationFacts()
            )
            // Drill-down: each durable step + the gate verdict.
            val stepLines = view.stepLines()
            assertTrue(stepLines.any { it == "[succeeded] push — pushed to origin" }, stepLines.toString())
            assertTrue(stepLines.any { it == "gate: satisfied" }, stepLines.toString())
            // The panel renders the proof node from the served DTO.
            val panel = TaskTreePanel()
            panel.update(model)
            assertEquals(model, panel.model())
            val proofChildren = childLabelsOf(panel, "goal/section:proof")
            assertTrue(
                proofChildren.contains("integration: ✓ Landed state matches verified state"),
                proofChildren.toString()
            )

            // An `unavailable` daemon verdict renders explicitly; the summary
            // NEVER carries the verified verdict.
            val unavailable =
                TaskTree.build(task = task, proof = parseNativeTaskProof(TASK_PROOF_UNAVAILABLE_JSON))
                    .proof ?: fail("unavailable proof view")
            assertEquals("unavailable", unavailable.state)
            val unavailableText = unavailable.summaryText()
            assertTrue(unavailableText.startsWith("VERIFICATION UNAVAILABLE"), unavailableText)
            assertEquals(1, unavailable.unavailable.size)
            assertEquals("landed_snapshot", unavailable.unavailable[0].component)
            assertEquals("mismatch", unavailable.unavailable[0].kind)
            assertTrue(
                unavailable.reason?.contains("landed_snapshot mismatch") == true,
                unavailable.reason ?: "(none)"
            )
            assertTrue(!unavailableText.contains("VERIFIED "), unavailableText)
            assertTrue(!unavailableText.contains("verified==landed: yes"), unavailableText)
            // The distinct integration facts: a landed/verified mismatch stays
            // a separate, explicit fact — never a success.
            assertEquals(listOf("✗ Mismatch"), unavailable.integrationFacts())

            // No landed identity is served: the candidate has not landed, so
            // the honest T18 fact is `Not integrated` (the served
            // `trees.landed` is only set from a non-empty landed snapshot).
            val notIntegrated = TaskTree.build(
                task = task,
                proof = parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(
                        "\"landed\":\"tm1:bbb\",\"landedEqualsVerified\":true",
                        "\"landed\":null,\"landedEqualsVerified\":null"
                    )
                )
            ).proof ?: fail("not-integrated proof view")
            assertEquals(listOf("○ Not integrated"), notIntegrated.integrationFacts())

            // The daemon's served in-flight integration marker (an
            // `integration_record` unavailable entry) renders `Integrating`,
            // never `Not integrated`.
            val integrating = TaskTree.build(
                task = task,
                proof = parseNativeTaskProof(
                    TASK_PROOF_JSON
                        .replace(
                            "\"landed\":\"tm1:bbb\",\"landedEqualsVerified\":true",
                            "\"landed\":null,\"landedEqualsVerified\":null"
                        )
                        .replace(
                            "\"unavailable\":[]",
                            "\"unavailable\":[{\"component\":\"integration_record\"," +
                                "\"kind\":\"unavailable\",\"reason\":\"the integration record " +
                                "is still in flight (no landed snapshot yet)\"}]"
                        )
                )
            ).proof ?: fail("integrating proof view")
            assertEquals(listOf("… Integrating"), integrating.integrationFacts())

            // A served integration record with recorded conflicts renders the
            // distinct `Conflict` fact (never folded into landed/mismatch),
            // exactly like the VS Code integration view; T19's exact
            // label+copy ride alongside the KEPT `! Conflict` fact.
            val conflictedModel = TaskTree.build(
                task = task,
                proof = parseNativeTaskProof(
                    TASK_PROOF_JSON.replace("\"conflictCount\":0", "\"conflictCount\":1")
                )
            )
            val conflicted = conflictedModel.proof ?: fail("conflicted proof view")
            val conflictFacts = conflicted.integrationFacts()
            assertEquals("! Conflict", conflictFacts[0])
            assertEquals(
                listOf(
                    RunStateCopy.WORKSPACE_CHANGED_LABEL,
                    RunStateCopy.WORKSPACE_CHANGED_COPY
                ),
                conflictFacts.drop(1)
            )
            val conflictPanel = TaskTreePanel()
            conflictPanel.update(conflictedModel)
            val conflictChildren = childLabelsOf(conflictPanel, "goal/section:proof")
            assertTrue(
                conflictChildren.contains("integration: ! Conflict"),
                conflictChildren.toString()
            )
            assertTrue(
                conflictChildren.contains("integration: " + RunStateCopy.WORKSPACE_CHANGED_LABEL),
                conflictChildren.toString()
            )
            assertTrue(
                conflictChildren.contains(
                    "integration: " + RunStateCopy.WORKSPACE_CHANGED_COPY
                ),
                conflictChildren.toString()
            )

            // The PARSED `integration.inFlight` marker renders `Integrating`
            // (the same fact as the legacy unavailable marker above).
            val inFlightView = TaskTree.build(
                task = task,
                proof = parseNativeTaskProof(
                    TASK_PROOF_JSON.replace("\"inFlight\":false", "\"inFlight\":true")
                )
            ).proof ?: fail("in-flight proof view")
            assertEquals(listOf("… Integrating"), inFlightView.integrationFacts())

            // The served record's own `present=false` is the T18
            // `Not integrated` state.
            val presentFalse = TaskTree.build(
                task = task,
                proof = parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(
                        "\"integration\":{\"present\":true",
                        "\"integration\":{\"present\":false"
                    )
                )
            ).proof ?: fail("present=false proof view")
            assertEquals(listOf("○ Not integrated"), presentFalse.integrationFacts())

            // Malformed integration members refuse the parse loudly (never a
            // fabricated state): a non-boolean `present` ...
            try {
                parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(
                        "\"integration\":{\"present\":true",
                        "\"integration\":{\"present\":\"yes\""
                    )
                )
                fail("a non-boolean integration.present must fail loudly")
            } catch (e: NativeProtocolException) {
                assertTrue(
                    e.message?.contains("integration.present") == true,
                    e.message ?: "(no message)"
                )
            }
            // ... and a served txn without its required phase.
            try {
                parseNativeTaskProof(
                    TASK_PROOF_JSON.replace(
                        "\"txn\":{\"txnId\":\"blake3:x\",\"phase\":\"landed\"}",
                        "\"txn\":{\"txnId\":\"blake3:x\"}"
                    )
                )
                fail("a txn without phase must fail loudly")
            } catch (e: NativeProtocolException) {
                assertTrue(
                    e.message?.contains("integration.txn") == true &&
                        e.message?.contains("phase") == true,
                    e.message ?: "(no message)"
                )
            }

            // An ABSENT block stays null (a legacy daemon): no fabricated
            // record, and the legacy tree-derived fact still renders.
            val absentBlock = parseNativeTaskProof(
                TASK_PROOF_JSON.replace("\"integration\":{", "\"integration_legacy\":{")
            )
            assertEquals(null, absentBlock.integration)
            val absentView = TaskTree.build(task = task, proof = absentBlock).proof
                ?: fail("absent-block proof view")
            assertEquals(
                listOf("✓ Landed state matches verified state"),
                absentView.integrationFacts()
            )
            assertTrue(
                absentView.integrationFacts()
                    .none { it.contains("Conflict") || it.contains("Integrating") },
                absentView.integrationFacts().toString()
            )

            // A failed read (the client refused the payload) is the same
            // explicit unavailable state: the reason is surfaced verbatim.
            val refused = TaskTree.build(
                task = task,
                proofUnavailable = "proof read failed: corrupt_durable_state"
            ).proof ?: fail("refused proof view")
            assertEquals("unavailable", refused.state)
            assertTrue(
                refused.summaryText().startsWith("VERIFICATION UNAVAILABLE"),
                refused.summaryText()
            )
            assertTrue(
                refused.reason?.contains("corrupt_durable_state") == true,
                refused.reason ?: "(none)"
            )
            assertTrue(!refused.summaryText().contains("VERIFIED "), refused.summaryText())
            assertEquals(
                listOf("? Integration unavailable — proof read failed: corrupt_durable_state"),
                refused.integrationFacts()
            )

            // The drill-down route parses a missing step honestly.
            val steps = parseNativeTaskCompletionSteps(COMPLETION_STEPS_JSON)
            assertEquals("faktor-task-completion-steps/v1", steps.schema)
            assertEquals(listOf("push", "pr"), steps.requestedSteps)
            assertEquals(false, steps.steps[1].present)
            assertEquals("missing", steps.steps[1].status)
            assertEquals("refused", steps.gateStatus)
            assertTrue(
                steps.gateReason?.contains("Pr") == true,
                steps.gateReason ?: "(none)"
            )
        }

        step("commercial metering parses: usage fold, entitlements, identity") {
            val usage = parseNativeBillingUsage(BILLING_USAGE_JSON)
            assertEquals("org-local", usage.organization)
            assertEquals(BigInteger.valueOf(700_000L), usage.fold.totals.managedCostMicro)
            assertEquals(BigInteger.valueOf(200_000L), usage.fold.totals.byokCostMicro)
            assertEquals(1_775L, usage.fold.totals.totalTokens())
            assertEquals(3L, usage.fold.perTask[0].taskId)
            assertEquals(BigInteger.valueOf(3_100_000L), usage.credits.balanceMicro())
            assertEquals(1, usage.itemCount)
            assertEquals("9", usage.nextCursor)
            val entitlements = parseNativeEntitlements(ENTITLEMENTS_JSON)
            assertEquals("pro", entitlements.planId)
            assertEquals(true, entitlements.planFound)
            assertEquals(true, entitlements.subscriptionActive)
            assertEquals(BigInteger.valueOf(100_000L), entitlements.limits[USAGE_LIMIT_MAX_TOKENS])
            assertEquals(4, entitlements.limits.size)
            assertEquals(1, entitlements.inFlight.size)
            assertEquals(null, entitlements.inFlight[0].endedMs)
            val identity = parseNativeIdentity(IDENTITY_JSON)
            assertEquals("admin", identity.role)
            assertEquals("org-local", identity.organization)
            assertEquals(true, identity.effectiveActions.contains("credits_grant"))
            assertEquals(false, parseNativeIdentity(MEMBER_IDENTITY_JSON).effectiveActions.contains("credits_grant"))
        }

        step("usage panel renders populated aggregates, credits and exact limits") {
            val model = usagePanelModelOf(
                parseNativeIdentity(IDENTITY_JSON),
                parseNativeEntitlements(ENTITLEMENTS_JSON),
                parseNativeBillingUsage(BILLING_USAGE_JSON),
                null, null, null, false
            )
            assertEquals("ok", model.state)
            assertEquals("org-local", model.organization)
            assertEquals("pro", model.planId)
            assertEquals(1_775L, model.totals?.totalTokens())
            assertEquals(BigInteger.valueOf(3_100_000L), model.credits?.balanceMicro())
            assertEquals(BigInteger.valueOf(250_000L), model.credits?.heldMicro)
            assertEquals("active", model.subscription)
            assertEquals(1, model.tasks.size)
            assertEquals(1, model.inFlight.size)
            assertEquals(1, model.itemCount)
            assertEquals("9", model.nextCursor)
            val tokensQuota = model.quotas.first { it.limit == USAGE_LIMIT_MAX_TOKENS }
            assertEquals(BigInteger.valueOf(100_000L), tokensQuota.value)
            assertEquals(BigInteger.valueOf(1_775L), tokensQuota.observed)
            assertEquals(false, tokensQuota.exceeded)
            // An unserved observed counter is null, never a fabricated zero.
            val unserved = model.quotas.first { it.limit == USAGE_LIMIT_ACTIVE_TASKS }
            assertEquals(null, unserved.observed)
            assertEquals(null, unserved.exceeded)
            val lines = model.lines()
            assertTrue(lines.any { it.contains("managed") && it.contains("BYOK") }, lines.toString())
            assertTrue(lines.any { it.contains("quota $USAGE_LIMIT_MAX_TOKENS") }, lines.toString())
            assertTrue(lines.any { it.contains("observed not served") }, lines.toString())
            assertTrue(lines.any { it.contains("subscription active") }, lines.toString())
            assertTrue(lines.any { it.contains("credits balance") }, lines.toString())
            assertTrue(lines.any { it.contains("org-local") }, lines.toString())
            val panel = UsagePanel()
            panel.setModel(model)
            assertEquals(true, panel.nextEnabled())
            assertEquals(false, panel.prevEnabled())
            assertEquals(true, panel.grantEnabled())
            assertTrue(panel.lines().any { it.contains("quota ") }, panel.lines().toString())
        }

        step("billing_disabled renders \"billing disabled locally\", never zeros") {
            val model = usagePanelModelOf(
                parseNativeIdentity(IDENTITY_JSON), null, null,
                "billing_disabled",
                "409 billing_disabled: commercial billing is disabled " +
                    "(enable the [billing] section to use it)",
                null, false
            )
            assertEquals("disabled", model.state)
            assertEquals(null, model.totals)
            assertEquals(null, model.credits)
            assertTrue(model.quotas.isEmpty(), "no quotas behind a disabled state")
            assertTrue(model.lines()[0].startsWith("billing disabled locally"), model.lines()[0])
            assertTrue(model.lines()[0].contains("billing_disabled"), model.lines()[0])
            assertTrue(
                model.lines().none { it.contains("\u00b5") },
                "no money is fabricated behind a disabled state: ${model.lines()}"
            )
            assertTrue(model.lines().none { it.contains("quota") }, model.lines().toString())
            assertEquals(false, model.grantEnabled())
            val panel = UsagePanel()
            panel.setModel(model)
            assertEquals(false, panel.nextEnabled())
            assertEquals(false, panel.prevEnabled())
            assertEquals(false, panel.grantEnabled())
        }

        step("expired subscription banner and quota-exceeded limit naming") {
            val entitlements = parseNativeEntitlements(ENTITLEMENTS_JSON)
            val usage = parseNativeBillingUsage(BILLING_USAGE_JSON)
            val identity = parseNativeIdentity(IDENTITY_JSON)
            val expired = usagePanelModelOf(
                identity,
                entitlements.copy(
                    subscriptionStatus = "expired",
                    subscriptionActive = false,
                    subscriptionExpiresMs = 1_700_000_000_000L
                ),
                usage, null, null, null, false
            )
            assertEquals("expired", expired.subscription)
            assertTrue(
                expired.lines().any {
                    it.startsWith("[EXPIRED]") && it.contains("new tasks are denied")
                },
                expired.lines().toString()
            )
            val lapsed = usagePanelModelOf(
                identity,
                entitlements.copy(subscriptionStatus = "active", subscriptionActive = false),
                usage, null, null, null, false
            )
            assertEquals("grace", lapsed.subscription)
            assertTrue(
                lapsed.lines().any { it.startsWith("[GRACE]") && it.contains("inactive") },
                lapsed.lines().toString()
            )
            val canceled = usagePanelModelOf(
                identity,
                entitlements.copy(subscriptionStatus = "canceled", subscriptionActive = false),
                usage, null, null, null, false
            )
            assertEquals("canceled", canceled.subscription)
            assertTrue(
                canceled.lines().any { it.startsWith("[CANCELED]") },
                canceled.lines().toString()
            )
            val over = usagePanelModelOf(
                identity,
                entitlements.copy(
                    totalTokens = 250_000L,
                    managedSpendMicro = BigInteger.valueOf(1_000_000L)
                ),
                usage, null, null, null, false
            )
            assertTrue(over.quotas.any { it.limit == USAGE_LIMIT_MAX_TOKENS && it.exceeded == true })
            assertTrue(over.quotas.any { it.limit == USAGE_LIMIT_MANAGED_SPEND && it.exceeded == true })
            val tokenLine = over.lines().first { it.contains(USAGE_LIMIT_MAX_TOKENS) }
            assertTrue(tokenLine.startsWith("[EXCEEDED] quota $USAGE_LIMIT_MAX_TOKENS"), tokenLine)
            assertTrue(tokenLine.contains("/ limit 100000"), tokenLine)
            val floor = usagePanelModelOf(
                identity,
                entitlements.copy(
                    credits = entitlements.credits.copy(
                        grantedMicro = BigInteger.valueOf(100L),
                        consumedMicro = BigInteger.valueOf(50L),
                        refundedMicro = BigInteger.ZERO
                    )
                ),
                usage, null, null, null, false
            )
            assertTrue(floor.quotas.any { it.limit == USAGE_LIMIT_MIN_CREDIT && it.exceeded == true })
            assertTrue(
                floor.lines().any { it.startsWith("[EXCEEDED] quota $USAGE_LIMIT_MIN_CREDIT") },
                floor.lines().toString()
            )
        }

        step("grant-credits affordance and cursor controls are role- and cursor-gated") {
            val identity = parseNativeIdentity(IDENTITY_JSON)
            val member = parseNativeIdentity(MEMBER_IDENTITY_JSON)
            val entitlements = parseNativeEntitlements(ENTITLEMENTS_JSON)
            val usage = parseNativeBillingUsage(BILLING_USAGE_JSON)
            val adminModel = usagePanelModelOf(identity, entitlements, usage, null, null, null, false)
            val memberModel = usagePanelModelOf(member, entitlements, usage, null, null, null, false)
            assertEquals(true, adminModel.grantEnabled())
            assertEquals(true, adminModel.canGrantCredits)
            assertEquals(null, adminModel.grantDisabledReason)
            assertTrue(
                adminModel.lines().any { it.contains("grants credits") },
                adminModel.lines().toString()
            )
            assertEquals(false, memberModel.grantEnabled())
            assertTrue(
                memberModel.grantDisabledReason?.contains("member") == true &&
                    memberModel.grantDisabledReason?.contains("credits_grant") == true,
                memberModel.grantDisabledReason ?: "(none)"
            )
            assertTrue(
                memberModel.lines().any { it.contains("grant credits disabled") },
                memberModel.lines().toString()
            )
            val events = ArrayList<String>()
            val panel = UsagePanel()
            panel.setListener(object : UsagePanel.Listener {
                override fun onNextPage() {
                    events.add("next")
                }

                override fun onPreviousPage() {
                    events.add("prev")
                }

                override fun onGrantCredits() {
                    events.add("grant")
                }
            })
            panel.setModel(memberModel)
            panel.clickGrant()
            assertEquals(0, events.size, "a disabled grant must never fire")
            panel.setModel(adminModel)
            panel.clickGrant()
            assertEquals(listOf("grant"), events)
            // Cursor paging: Next fires while served, Previous only with a
            // cursor stack; a disabled control never fires.
            panel.setModel(adminModel)
            assertEquals(true, panel.nextEnabled())
            assertEquals(false, panel.prevEnabled())
            panel.clickPrevious()
            assertEquals(listOf("grant"), events)
            panel.clickNext()
            assertEquals(listOf("grant", "next"), events)
            val second = usagePanelModelOf(
                identity,
                entitlements,
                usage.copy(nextCursor = null),
                null, null, "9", true
            )
            panel.setModel(second)
            assertEquals(false, panel.nextEnabled())
            assertEquals(true, panel.prevEnabled())
            assertTrue(
                second.lines().any { it.contains("cursor 9") && it.contains("next none") },
                second.lines().toString()
            )
            panel.clickNext()
            assertEquals(listOf("grant", "next"), events, "a disabled Next must never fire")
            panel.clickPrevious()
            assertEquals(listOf("grant", "next", "prev"), events)
            assertEquals("9", second.cursor)
        }

        step("malformed billing payloads are loud protocol violations") {
            for (text in listOf(
                "{}",
                "{\"ok\":true}",
                "{\"organization\":\"o\",\"fold\":{\"organization_id\":\"o\"," +
                    "\"totals\":{\"input_tokens\":\"many\"},\"per_task\":[]," +
                    "\"next_cursor\":null},\"credits\":{},\"items\":[],\"nextCursor\":null}"
            )) {
                try {
                    parseNativeBillingUsage(text)
                    fail("hostile billing usage must be rejected: $text")
                } catch (e: NativeProtocolException) {
                    // expected: the panel renders an explicit unavailable state
                }
            }
            try {
                parseNativeEntitlements("{\"ok\":true,\"entitlements\":{}}")
                fail("a malformed entitlement snapshot must be rejected")
            } catch (e: NativeProtocolException) {
                // expected
            }
            try {
                parseNativeIdentity("{\"ok\":true,\"identity\":{}}")
                fail("a malformed identity must be rejected")
            } catch (e: NativeProtocolException) {
                // expected
            }
            val malformed = usagePanelModelOf(
                null, null, null,
                "malformed", "GET /native/usage: missing required field credits",
                null, false
            )
            assertEquals("unavailable", malformed.state)
            assertEquals(null, malformed.totals)
            assertEquals(null, malformed.credits)
            assertTrue(
                malformed.lines()[0].startsWith("usage unavailable"),
                malformed.lines()[0]
            )
            assertTrue(
                malformed.lines().none { it.contains("\u00b5") },
                "no fabricated numbers: ${malformed.lines()}"
            )
        }

        step("money is exact: string forms, unsafe-number refusal, display, aggregation") {
            val identity = parseNativeIdentity(IDENTITY_JSON)
            val bigUsage = parseNativeBillingUsage(
                moneyUsageJson(
                    managed = "9223372036854775806",
                    granted = "9223372036854775807"
                )
            )
            val bigEntitlements = parseNativeEntitlements(
                moneyEntitlementsJson(
                    managed = "9223372036854775806",
                    limit = "9223372036854775807",
                    floor = "9007199254740993"
                )
            )
            val model = usagePanelModelOf(
                identity, bigEntitlements, bigUsage, null, null, null, false
            )
            assertEquals("ok", model.state)
            assertEquals(MicroMoney.I64_MAX, model.credits?.balanceMicro())
            val lines = model.lines()
            assertTrue(
                lines.any { it.contains("credits balance 9223372036854775807\u00b5\$") },
                lines.toString()
            )
            assertTrue(
                lines.any { it.contains("managed 9223372036854775806\u00b5\$") },
                lines.toString()
            )
            assertTrue(
                lines.none { it.contains("E") && it.contains("\u00b5") },
                "money display must never use scientific notation: $lines"
            )
            val managed = model.quotas.first { it.limit == USAGE_LIMIT_MANAGED_SPEND }
            assertEquals(BigInteger("9223372036854775806"), managed.observed)
            assertEquals(MicroMoney.I64_MAX, managed.value)
            assertEquals(false, managed.exceeded)
            val floor = model.quotas.first { it.limit == USAGE_LIMIT_MIN_CREDIT }
            assertEquals(false, floor.exceeded)

            // 2^53 vs 2^53+1: a float parse would collapse the pair and flip
            // the verdict; the exact BigInteger compare keeps them apart.
            val near = usagePanelModelOf(
                identity,
                parseNativeEntitlements(
                    moneyEntitlementsJson(
                        managed = "9007199254740992",
                        limit = "9007199254740993",
                        floor = "0"
                    )
                ),
                parseNativeBillingUsage(
                    moneyUsageJson(managed = "9007199254740992", granted = "9007199254740992")
                ),
                null, null, null, false
            )
            val nearQuota = near.quotas.first { it.limit == USAGE_LIMIT_MANAGED_SPEND }
            assertEquals(BigInteger("9007199254740992"), nearQuota.observed)
            assertEquals(BigInteger("9007199254740993"), nearQuota.value)
            assertEquals(false, nearQuota.exceeded)

            // Aggregation: two i64::MAX spends sum exactly (no Long overflow).
            val sum = TaskTree.build(
                usage = parseNativeSessionUsage(
                    "{\"sessionId\":\"1\",\"providerCalls\":{\"tokens\":0," +
                        "\"prefixObservations\":[]},\"prefixStability\":null,\"tasks\":[" +
                        "{\"taskId\":\"3\",\"budget\":{\"spentCostMicro\":" +
                        "\"9223372036854775807\",\"openReservedMicro\":\"1\"}}," +
                        "{\"taskId\":\"4\",\"budget\":{\"spentCostMicro\":" +
                        "\"9223372036854775807\",\"openReservedMicro\":\"1\"}}]}"
                )
            )
            assertEquals(
                BigInteger("18446744073709551614"),
                sum.spend?.spentCostMicro,
                "exact BigInteger aggregation past the Long range"
            )
            assertEquals(BigInteger.valueOf(2L), sum.spend?.openReservedMicro)
        }

        step("every new UI section is constructible from the mock payload") {
            val model = TaskTree.build(
                task = parseNativeTaskViews(TASK_JSON)[0],
                agents = parseNativeAgents(AGENTS_JSON),
                catalog = parseNativeModelCatalog(MODELS_JSON),
                verification = parseNativeVerificationView(VERIFICATION_JSON),
                taskVerification = parseNativeTaskVerification(TASK_VERIFICATION_JSON),
                usage = parseNativeSessionUsage(SESSION_USAGE_JSON),
                childUsage = mapOf("9" to parseNativeSessionUsage(SESSION_USAGE_JSON)),
                tournament = parseNativeTournament(TOURNAMENT_JSON)
            )
            val tree = TaskTreePanel()
            tree.update(model)
            assertEquals(model, tree.model())
            assertTrue(
                tree.childLabel(model.children[0]).contains("child-1"),
                "child label must surface the child id"
            )
            assertTrue(
                tree.childLabel(model.children[0]).contains("(background)"),
                "background children must be dimmed/marked in the tree"
            )
            val blockers = BlockersPanel()
            blockers.update(model.blockers, parseNativePermissionList(PERMISSION_LIST_JSON), model.taskBlockers)
            assertEquals(1, blockers.blockerCount())
            assertEquals(1, blockers.permissionCount())
            assertTrue(
                blockers.applicableActions().contains(BlockerAction.PERMISSION_DENY),
                "selected blocker actions must be visible"
            )
            val tournament = TournamentPanel()
            tournament.setTournament(
                TaskTree.tournamentView(parseNativeTournament(TOURNAMENT_JSON))
            )
            assertEquals(2, tournament.candidateCount())
            val usagePanel = UsagePanel()
            usagePanel.setModel(
                usagePanelModelOf(
                    parseNativeIdentity(IDENTITY_JSON),
                    parseNativeEntitlements(ENTITLEMENTS_JSON),
                    parseNativeBillingUsage(BILLING_USAGE_JSON),
                    null, null, null, false
                )
            )
            assertEquals("ok", usagePanel.model()?.state)
            assertTrue(usagePanel.lines().isNotEmpty(), "the usage panel must render lines")
            val navigator = EvidenceNavigatorPanel()
            navigator.setEvidence(model.evidence)
            navigator.setMessages(
                listOf(
                    NativeMessage(seq = 1, id = 1, role = "user", createdMs = 1, text = "hi"),
                    NativeMessage(seq = 2, id = 2, role = "assistant", createdMs = 2, text = "hello")
                )
            )
            assertEquals(3, navigator.evidenceCount())
            assertEquals(2, navigator.messageCount())
            assertEquals("{\"selector\":\"all\"}", navigator.selectorJson())
            val workspace = Files.createTempDirectory("faktor-attach-smoke-")
            val srcDir = Files.createDirectory(Paths.get(workspace.toString(), "src"))
            val file = Files.createFile(Paths.get(srcDir.toString(), "note.txt"))
            val attachments = AttachmentsPanel(workspace)
            attachments.addFiles(listOf(file.toString(), file.toString()))
            assertEquals(1, attachments.count())
            assertEquals(1, attachments.files().size)
            assertEquals(
                "src/note.txt",
                attachments.files()[0],
                "a picked absolute path inside the workspace is submitted relative"
            )
            assertEquals(0, attachments.refusalCount(), "an inside-workspace pick is never refused")
            // An outside-workspace pick is refused TYPED and stays visible in
            // the refusal list; it never enters the submitted path list.
            val outside = Files.createTempFile("faktor-attach-outside-", ".txt")
            attachments.addFiles(listOf(outside.toString()))
            assertEquals(1, attachments.count(), "an outside-workspace entry is never staged")
            assertEquals(1, attachments.refusalCount())
            assertEquals("outside_workspace", attachments.refusals()[0].code)
            assertTrue(
                attachments.refusals()[0].message!!.contains("outside the session workspace"),
                attachments.refusals()[0].message ?: "no message"
            )
            // Without a known workspace root every path entry is refused
            // typed instead of being forwarded absolute (the daemon refuses
            // absolute attached files).
            val rootless = AttachmentsPanel()
            rootless.addFiles(listOf(file.toString()))
            assertEquals(0, rootless.count())
            assertEquals("workspace_unknown", rootless.refusals()[0].code)
            // Traversal and control characters are refused with the VS Code
            // client's vocabulary, never normalized into the workspace.
            attachments.addFiles(listOf("../escape.rs", "bad\u0001name.rs"))
            assertEquals(3, attachments.refusalCount())
            assertEquals("outside_workspace", attachments.refusals()[1].code)
            assertEquals("invalid_path", attachments.refusals()[2].code)
            attachments.clear()
            assertEquals(0, attachments.count())
            assertEquals(0, attachments.refusalCount(), "clear also drops the refusals")
            rootless.clear()
            // Image parity: the classifier mirrors the daemon allowlist and
            // the bounded read refuses an oversized image before base64.
            assertEquals("image/png", AttachmentImages.mimeOf("shot.PNG"))
            assertEquals("image/jpeg", AttachmentImages.mimeOf(workspace.toString() + "/a.jpeg"))
            assertTrue(AttachmentImages.mimeOf(workspace.toString() + "/spec.pdf") == null)
            val shot = Files.createTempFile("faktor-attach-image-", ".png")
            Files.write(shot, byteArrayOf(1, 2, 3, 4))
            val read = AttachmentImages.readBounded(shot.toFile())
            assertTrue(read != null && read.size == 4, "the bounded image read is byte-exact")
            assertTrue(
                AttachmentImages.readBounded(shot.toFile(), 3L) == null,
                "an over-bound image is refused before any upload"
            )
            val panel = FaktorChatPanel(
                FaktorFrontendService(
                    Paths.get("target/debug/faktor-cli"),
                    Paths.get(System.getProperty("java.io.tmpdir"), "faktor-frontend-smoke")
                )
            )
            panel.shutdown()
        }

        // Audit 29 + audit finding 4: the JetBrains pending-upload state
        // retains the COMPLETE durable reference (ref_id included), reuses it
        // on a retry, uploads only absent attachments, and never reuses an
        // entry across sessions.
        step("pending attachment retry retains the full reference and only uploads absent entries") {
            val retry = PendingAttachmentRetry(maxEntries = 2)
            val ref = { seed: String, refId: Long ->
                ProtocolAttachmentRef(
                    refId = refId,
                    digest = seed.repeat(64),
                    mime = "image/png",
                    filename = "shot.png",
                    size = 4
                )
            }
            var uploads = 0
            val keys = listOf("image:/w/a.png:4:1:image/png", "image:/w/b.png:4:1:image/png")
            val first = retry.resolve("7", keys) { index ->
                uploads++
                ref(if (index == 0) "a" else "b", if (index == 0) 11L else 12L)
            }
            assertEquals(2, first.size)
            assertEquals(2, uploads, "the first attempt uploads every absent entry")
            assertEquals("a".repeat(64), first[0].digest)
            assertEquals(
                11L,
                retry.reusable("7", keys[0])?.refId,
                "the pending state keeps the uploaded reference identity"
            )
            val retryIds = retry.resolve("7", keys) {
                uploads++
                ref("z", 99L)
            }
            assertEquals(2, uploads, "the retry resolves retained references and uploads nothing")
            assertEquals("b".repeat(64), retryIds[1].digest)
            assertEquals(
                12L,
                retry.reusable("7", keys[1])?.refId,
                "a reused reference keeps its own ref id"
            )
            retry.resolve("8", keys) {
                uploads++
                ref("c", 13L)
            }
            assertEquals(4, uploads, "an entry retained for session 7 is never reused by session 8")
            assertEquals("c".repeat(64), retry.reusable("8", keys[0])?.digest)
            assertEquals(13L, retry.reusable("8", keys[0])?.refId)
            retry.release("8", keys)
            assertEquals(0, retry.size(), "durable acceptance releases the pending ids")
            retry.retain("9", "k1", ref("d", 14L))
            retry.retain("9", "k2", ref("e", 15L))
            retry.retain("9", "k3", ref("f", 16L))
            assertEquals(2, retry.size(), "pending retry state is bounded")
            assertEquals(null, retry.reusable("9", "k1"), "the oldest pending upload was evicted")
        }

        // Audit 9/10 + attachment-reference identity: the retry identity is
        // kind + mime + FILENAME + SIZE + byte digest. Path/length/mtime are
        // metadata only, so same-size same-mtime different bytes upload
        // fresh; identical bytes under a different filename are a DIFFERENT
        // reference and also upload fresh, while the same reference at
        // another path keeps its id.
        step("path attachment identity is kind+mime+filename+size+byte digest, never path or mtime") {
            val dir = Files.createTempDirectory("faktor-attach-identity-")
            val file = Paths.get(dir.toString(), "doc.txt")
            val bytesA = "AAAA".toByteArray()
            val bytesB = "BBBB".toByteArray()
            val stamp = 1_700_000_000_000L
            val policy = AttachmentImages.emergencyPolicy()
            Files.write(file, bytesA)
            file.toFile().setLastModified(stamp)
            val keyA = planAttachments(listOf(file.toString()), emptyList(), policy, dir)
                .uploads.single().key
            // Same path, same length, same mtime; different bytes => fresh identity.
            Files.write(file, bytesB)
            file.toFile().setLastModified(stamp)
            val keyB = planAttachments(listOf(file.toString()), emptyList(), policy, dir)
                .uploads.single().key
            assertTrue(
                keyA != keyB,
                "same-size same-mtime changed bytes must not reuse the old identity"
            )
            // Identical bytes AND the same filename at another path/time keep
            // the SAME reference identity.
            val otherDir = Files.createDirectory(Paths.get(dir.toString(), "other"))
            val twin = Paths.get(otherDir.toString(), "doc.txt")
            Files.write(twin, bytesB)
            twin.toFile().setLastModified(stamp + 1)
            val keyTwin = planAttachments(listOf(twin.toString()), emptyList(), policy, dir)
                .uploads.single().key
            assertEquals(keyB, keyTwin, "identity is content+filename, not path or mtime")
            // Identical bytes under a DIFFERENT filename is a distinct
            // reference: a rename/re-select must upload fresh.
            val renamed = Paths.get(otherDir.toString(), "renamed.txt")
            Files.write(renamed, bytesB)
            val keyRenamed = planAttachments(listOf(renamed.toString()), emptyList(), policy, dir)
                .uploads.single().key
            assertTrue(
                keyB != keyRenamed,
                "identical bytes under a different filename must be a fresh reference"
            )
            // The identity is exactly
            // kind:mime:name=<filename>:size=<n>:sha=<digest>; the PATH never
            // appears.
            val digestHex = java.security.MessageDigest.getInstance("SHA-256")
                .digest(bytesB).joinToString("") { "%02x".format(it.toInt() and 0xff) }
            assertEquals(
                "document:text/plain:name=doc.txt:size=${bytesB.size}:sha=$digestHex",
                keyB,
                "the retry key shape"
            )
            // Adversarial: the size-less shape (the one this smoke used to
            // assert) must never equal the real key — if pathUploadKey ever
            // drops the size field, both assertions fail loudly.
            assertTrue(
                keyB != "document:text/plain:name=doc.txt:sha=$digestHex",
                "the retry key must carry the size field, never the old size-less shape"
            )
            // The size field is part of the identity: same filename, same
            // bytes-prefix, one byte more => a fresh reference.
            assertTrue(
                pathUploadKey("document", "text/plain", "doc.txt", bytesB + "!".toByteArray()) !=
                    keyB,
                "a changed size must change the retry key"
            )
            assertTrue(!keyB.contains(file.toString()), "the path stays metadata only")
        }

        // Audit 11: the paste binding is the platform menu shortcut (the
        // toolkit's own mask: Command+V on macOS, Ctrl+V elsewhere), never a
        // hardcoded Ctrl stroke. Headless there is no toolkit display: the
        // binding must still exist on the documented Ctrl/X11 fallback mask,
        // and the toolkit mask itself is only asserted when a toolkit exists.
        step("paste shortcut binds the platform menu-shortcut mask") {
            val headless = GraphicsEnvironment.isHeadless()
            val panel = AttachmentsPanel()
            val mask = menuShortcutMask()
            val bound = panel.pasteKeyStroke() ?: fail("the panel must install a paste binding")
            assertEquals(
                KeyStroke.getKeyStroke(KeyEvent.VK_V, mask),
                bound,
                "the binding is VK_V with the platform menu-shortcut mask"
            )
            assertEquals(KeyEvent.VK_V, bound.keyCode)
            assertEquals(false, bound.isOnKeyRelease)
            assertTrue(
                (bound.modifiers and mask) == mask,
                "the bound stroke carries the platform menu-shortcut mask"
            )
            if (headless) {
                // The headless platform contract is the documented Ctrl mask
                // (Linux/X11): the same helper the binding used, so a silent
                // headless toolkit call regression fails here.
                assertEquals(
                    InputEvent.CTRL_DOWN_MASK,
                    mask,
                    "headless must bind the documented Ctrl fallback mask"
                )
            } else {
                assertEquals(
                    Toolkit.getDefaultToolkit().menuShortcutKeyMaskEx,
                    mask,
                    "headful must bind the toolkit's own menu-shortcut mask"
                )
                // Where the menu shortcut is NOT Ctrl (macOS: Command), the
                // old hardcoded `ctrl V` stroke must not be what is installed.
                if (mask != InputEvent.CTRL_DOWN_MASK) {
                    assertTrue(
                        bound != KeyStroke.getKeyStroke(KeyEvent.VK_V, InputEvent.CTRL_DOWN_MASK),
                        "a non-Ctrl platform must not bind plain Ctrl+V"
                    )
                }
            }
            assertTrue(
                panel.actionMap.get("faktor-paste-image") != null,
                "the paste action is wired to the bound stroke"
            )
        }

        // Clipboard images (audit 12): a BufferedImage converts to bounded
        // PNG bytes IN MEMORY (no filesystem path, no temp file) and stages
        // separately from the file-path list as a pending binary.
        step("clipboard image: BufferedImage converts to bounded in-memory PNG (never a file)") {
            val image = BufferedImage(3, 2, BufferedImage.TYPE_INT_ARGB)
            image.setRGB(0, 0, 0xFF112233.toInt())
            image.setRGB(1, 1, 0xFF445566.toInt())
            val bytes = AttachmentImages.pngBytes(image) ?: fail("the PNG conversion refused")
            assertTrue(bytes.size > 8, "PNG bytes must carry the signature + payload")
            assertEquals(0x89.toByte(), bytes[0], "PNG signature byte 0")
            assertEquals('P'.toInt().toByte(), bytes[1], "PNG signature byte 1")
            assertEquals('N'.toInt().toByte(), bytes[2], "PNG signature byte 2")
            assertEquals('G'.toInt().toByte(), bytes[3], "PNG signature byte 3")
            val decoded = ImageIO.read(ByteArrayInputStream(bytes))
                ?: fail("the in-memory PNG must decode")
            assertEquals(3, decoded.width)
            assertEquals(2, decoded.height)
            // The bounded encoder refuses above the byte bound and the pixel
            // bound; a hostile huge Image is refused before any allocation.
            assertEquals(null, AttachmentImages.pngBytes(image, 8L), "over-bound PNG bytes refuse")
            val huge = object : java.awt.Image() {
                override fun getWidth(observer: java.awt.image.ImageObserver?): Int = 100_000
                override fun getHeight(observer: java.awt.image.ImageObserver?): Int = 100_000
                override fun getSource(): java.awt.image.ImageProducer =
                    throw IllegalStateException("no source")
                override fun getGraphics(): java.awt.Graphics =
                    throw IllegalStateException("no graphics")
                override fun getProperty(name: String, observer: java.awt.image.ImageObserver?): Any = ""
                override fun getScaledInstance(w: Int, h: Int, hints: Int): java.awt.Image = this
            }
            assertEquals(null, AttachmentImages.pngBytes(huge), "the pixel bound refuses a huge Image")
            // The panel stages the bytes separately from paths and never
            // creates a file for them.
            val panel = AttachmentsPanel()
            if (GraphicsEnvironment.isHeadless()) {
                // No OS clipboard exists headless: the paste path must answer
                // the explicit unavailable refusal (never throw, never stage),
                // while the in-memory PNG conversion below stays exercised.
                val refusal = panel.pasteImageFromClipboard()
                assertTrue(
                    refusal != null && refusal.contains("unavailable"),
                    "a headless clipboard paste must return the explicit refusal"
                )
                assertEquals(0, panel.binaryCount(), "a refused paste stages nothing")
            }
            assertTrue(panel.addClipboardImage(image, "clip-shot.png"))
            assertEquals(1, panel.binaryCount())
            assertEquals(1, panel.count())
            assertEquals(0, panel.files().size, "binaries never enter the path list")
            val staged = panel.binaryAttachments()[0]
            assertEquals("image/png", staged.mime)
            assertEquals("clip-shot.png", staged.filename)
            val stagedBase64 = staged.base64()
            assertEquals(
                true,
                java.util.Base64.getDecoder().decode(stagedBase64).contentEquals(bytes),
                "the staged bytes are the exact in-memory PNG"
            )
            assertEquals(false, panel.addBinary("image/png", "empty.png", ByteArray(0)))
            panel.clear()
            assertEquals(0, panel.count())
            assertEquals(0, panel.binaryCount())
        }

        // Document delivery (audit 13): the advertised document contract
        // drives the plan; ordinary source files stay WORKSPACE-RELATIVE
        // paths (absolute picks inside the root are relativized, never sent
        // absolute); outside-workspace entries are typed refusals before any
        // upload, exactly like unsupported MIME / oversize entries.
        step("attachment plan: documents upload when advertised, source files stay workspace-relative") {
            val root = System.getProperty("faktor.repo.root")
            val fixturePath =
                if (root.isNullOrBlank()) Paths.get("fixtures", "attachment-limits.json")
                else Paths.get(root, "fixtures", "attachment-limits.json")
            val fixture = JsonCodec.parse(Files.readString(fixturePath)).view("fixture")
            val canonical = fixture.field("canonical").rawJson()
            val catalog = parseNativeModelCatalog(
                "[{\"provider\":\"fake\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
                    "\"tools\":true,\"parallelTools\":false,\"reasoning\":true,\"thinking\":true," +
                    "\"vision\":true,\"structuredOutput\":false,\"embeddings\":false," +
                    "\"streaming\":true,\"source\":\"conservativeDefault\"," +
                    "\"documentCapable\":true,\"attachmentLimits\":" + canonical + "}]"
            )
            val policy = AttachmentImages.policyForModel(catalog, "fake", "m")
            val dir = Files.createTempDirectory("faktor-plan-smoke-")
            val pdf = Paths.get(dir.toString(), "spec.pdf")
            Files.write(pdf, "%PDF-1.4".toByteArray())
            val txt = Paths.get(dir.toString(), "notes.txt")
            Files.write(txt, "hello".toByteArray())
            val source = Paths.get(dir.toString(), "Main.kt")
            Files.write(source, "fun main() {}".toByteArray())
            val image = BufferedImage(2, 2, BufferedImage.TYPE_INT_ARGB)
            val binary = PendingBinaryAttachment(
                "image/png", "clip.png", AttachmentImages.pngBytes(image)!!
            )
            val plan = planAttachments(
                listOf(pdf.toString(), txt.toString(), source.toString(), "Makefile"),
                listOf(binary),
                policy,
                dir
            )
            assertEquals(
                listOf(binary.mime, "application/pdf", "text/plain"),
                plan.uploads.map { it.mime },
                "binaries first, then deliverable documents (images must exist on disk)"
            )
            assertEquals(
                listOf("Main.kt", "Makefile"),
                plan.pathFiles,
                "absolute picks inside the root are relativized; relative entries stay relative"
            )
            assertTrue(
                plan.uploads.all { it.base64.isNotEmpty() && it.key.isNotEmpty() },
                "every upload is keyed and base64-ready"
            )
            // A legacy (emergency) policy still uploads documents: the daemon
            // decides admission until the catalog is advertised.
            val emergency = planAttachments(
                listOf(pdf.toString(), txt.toString()), emptyList(),
                AttachmentImages.emergencyPolicy(), dir
            )
            assertEquals(2, emergency.uploads.size)
            assertEquals(0, emergency.pathFiles.size)
            // Outside the workspace: an ordinary file is refused typed and
            // never becomes a `files` entry, and an image upload is refused
            // too (the session workspace is the one admitted root).
            val outside = Files.createTempFile("faktor-plan-outside-", ".kt")
            val outsideRefusal = refusalOf {
                planAttachments(listOf(outside.toString()), emptyList(), policy, dir)
            }
            assertEquals("outside_workspace", outsideRefusal.code)
            // Without a known workspace root NO path entry can be admitted:
            // the absolute form would deterministically be refused by the
            // daemon, so the client refuses it typed before any request.
            val rootlessRefusal = refusalOf {
                planAttachments(listOf(txt.toString()), emptyList(), policy)
            }
            assertEquals("workspace_unknown", rootlessRefusal.code)
            // Unsupported document MIME: the advertised list lacks text/plain.
            val pdfOnly = policy.copy(documentMimes = listOf("application/pdf"))
            val mimeRefusal = refusalOf {
                planAttachments(listOf(txt.toString()), emptyList(), pdfOnly, dir)
            }
            assertEquals("unsupported_document_type", mimeRefusal.code)
            assertTrue(
                mimeRefusal.message!!.contains("deliverable document types"),
                mimeRefusal.message ?: "no message"
            )
            // Advertised model without document input.
            val incapable = refusalOf {
                planAttachments(
                    listOf(pdf.toString()), emptyList(), policy.copy(documentCapable = false), dir
                )
            }
            assertEquals("unsupported_document_type", incapable.code)
            assertTrue(
                incapable.message!!.contains("does not advertise document input"),
                incapable.message ?: "no message"
            )
            // Oversized document: refused above the advertised bound.
            val oversized = refusalOf {
                planAttachments(
                    listOf(pdf.toString()), emptyList(),
                    policy.copy(maxDocumentBytes = 4L), dir
                )
            }
            assertEquals("oversized_document", oversized.code)
            assertTrue(
                oversized.message!!.contains("per-document bound"),
                oversized.message ?: "no message"
            )
            // Unsupported image MIME under the advertised allowlist.
            val noPng = policy.copy(imageMimes = listOf("image/jpeg"))
            val imageRefusal = refusalOf {
                planAttachments(emptyList(), listOf(binary), noPng)
            }
            assertEquals("unsupported_image_type", imageRefusal.code)
            // An oversize UNDELIVERABLE entry refuses before any upload, so
            // the plan builder never returns a partial plan.
            val partial = runCatching {
                planAttachments(
                    listOf(pdf.toString(), txt.toString()), emptyList(),
                    policy.copy(maxDocumentBytes = 4L), dir
                )
            }
            assertTrue(partial.isFailure, "a refused entry fails the whole plan")
        }

        // P1: compact areas derive from IDE typography; monospace is opt-in.
        step("compact area typography follows the IDE font; monospace is opt-in") {
            val ui = compactArea(2)
            val mono = compactArea(2, monospace = true)
            assertEquals(uiPanelFont(), ui.font, "default text must use the IDE UI font")
            assertEquals(monospacePanelFont(), mono.font, "monospace must be explicit opt-in")
            val context = java.awt.font.FontRenderContext(null, false, false)
            val narrow = mono.font.getStringBounds("i", context).width
            val wide = mono.font.getStringBounds("W", context).width
            assertTrue(Math.abs(narrow - wide) < 0.5, "the opt-in font must actually be monospaced")
        }

        // P0: every blocker/permission action is attached to its OWN card;
        // there is no shared action bar driven by hidden selection precedence.
        step("blockers panel: each item carries its own inline controls, no shared bar") {
            val permissionBlocker = BlockerRow(
                childId = "child-1",
                kind = "permission",
                reason = "shell call needs approval",
                dependency = null,
                resolution = "allow the shell tool",
                lastProgressMs = 42L,
                presence = PixelAgents.presence("child-1", "Blocked"),
                actions = listOf(
                    BlockerAction.RESUME,
                    BlockerAction.PERMISSION_ALLOW,
                    BlockerAction.PERMISSION_DENY,
                    BlockerAction.RETRY
                )
            )
            val dependencyBlocker = BlockerRow(
                childId = "child-2",
                kind = "dependency",
                reason = "waiting on impl-a",
                dependency = "impl-a",
                resolution = "wait for impl-a",
                lastProgressMs = 7L,
                presence = PixelAgents.presence("child-2", "Blocked"),
                actions = listOf(BlockerAction.RESUME, BlockerAction.RETRY)
            )
            val permission7 = NativePermissionEntry("7", "9", "shell", "{\"tool\":\"bash\"}")
            val permission8 = NativePermissionEntry("8", "9", "network", "{\"host\":\"example\"}")
            val panel = BlockersPanel()
            val routed = ArrayList<String>()
            panel.setListener(object : BlockersPanel.Listener {
                override fun onBlockerAction(row: BlockerRow, action: BlockerAction) {
                    routed.add("blocker:${row.childId}:${action.name}")
                }

                override fun onPermissionReply(permission: NativePermissionEntry, decision: String) {
                    routed.add("permission:${permission.id}:$decision")
                }
            })
            panel.update(
                listOf(permissionBlocker, dependencyBlocker),
                listOf(permission7, permission8),
                listOf("waiting on review")
            )
            assertEquals(2, panel.blockerCount())
            assertEquals(2, panel.permissionCount())
            // Each blocker card has its OWN distinct button objects; the
            // allow/deny actions live on the permission cards, not on a
            // shared bar.
            val child1Retry = panel.blockerActionButton("child-1", BlockerAction.RETRY)
                ?: fail("child-1 card must carry its own Retry")
            val child2Retry = panel.blockerActionButton("child-2", BlockerAction.RETRY)
                ?: fail("child-2 card must carry its own Retry")
            assertTrue(child1Retry !== child2Retry, "cards must not share one button instance")
            assertEquals(
                null,
                panel.blockerActionButton("child-1", BlockerAction.PERMISSION_ALLOW),
                "allow/deny belong to the permission cards, not the blocker cards"
            )
            val allow7 = panel.permissionActionButton("7", "allow")
                ?: fail("permission 7 must carry its own Allow")
            val deny8 = panel.permissionActionButton("8", "deny")
                ?: fail("permission 8 must carry its own Deny")
            assertTrue(allow7 !== deny8, "permission cards must not share one button instance")

            // Acting on one card NEVER affects another: no selection exists to
            // become ambiguous with nothing highlighted.
            child1Retry.doClick(0)
            assertEquals(listOf("blocker:child-1:RETRY"), routed)
            child2Retry.doClick(0)
            assertEquals(listOf("blocker:child-1:RETRY", "blocker:child-2:RETRY"), routed)
            allow7.doClick(0)
            deny8.doClick(0)
            assertEquals(
                listOf(
                    "blocker:child-1:RETRY",
                    "blocker:child-2:RETRY",
                    "permission:7:allow",
                    "permission:8:deny"
                ),
                routed
            )
            ParityAwt.layout(panel, 480, 600)
        }

        // P0: faithful argv entry — one argument per line, never a space split.
        step("terminal composer: spaces stay inside one argv element; shell mode is explicit") {
            val panel = TerminalPanel()
            var command: String? = null
            var args: List<String>? = null
            var cwd: String? = null
            var loadedAfter: Long? = null
            panel.setListener(object : TerminalPanel.Listener {
                override fun onRefresh() {}
                override fun onSpawn(c: String, a: List<String>, w: String?) {
                    command = c
                    args = a
                    cwd = w
                }

                override fun onOutput(ptyId: String) {}
                override fun onLoadEvents(after: Long) {
                    loadedAfter = after
                }
            })
            panel.setComposerFields("echo", "hello world\n-c", "")
            panel.submitSpawn()
            assertEquals("echo", command)
            assertEquals(listOf("hello world", "-c"), args, "spaces must stay inside one argument")
            assertEquals(null, cwd)
            // Every non-empty line is one argument, verbatim (no trim).
            panel.setComposerFields("echo", "  padded  \n\n", "")
            panel.submitSpawn()
            assertEquals(listOf("  padded  "), args)
            // Explicit shell mode: the labeled command goes to the platform
            // shell as ONE argument; it is never word-split by the panel.
            assertEquals(false, panel.shellModeSelected())
            panel.setShellMode(true)
            panel.setShellCommand("echo 'hello world'")
            panel.submitSpawn()
            if (System.getProperty("os.name", "").lowercase().contains("win")) {
                assertEquals("cmd.exe", command)
                assertEquals(listOf("/c", "echo 'hello world'"), args)
            } else {
                assertEquals("/bin/sh", command)
                assertEquals(listOf("-lc", "echo 'hello world'"), args)
            }
            // Event paging: the has_more hint is backed by a real control.
            panel.setEvents(
                parseNativeTerminalEventPage(
                    "{\"sessionId\":\"7\",\"events\":[{\"id\":1,\"type\":\"created\"," +
                        "\"ptyId\":\"5\",\"pid\":123,\"tsMs\":1700,\"sessionId\":\"7\"}]," +
                        "\"hasMore\":true,\"nextCursor\":42}"
                )
            )
            assertEquals(true, panel.loadMoreEventsEnabled(), "has_more must arm Load more events")
            panel.loadMoreEvents()
            assertEquals(42L, loadedAfter)
            panel.appendEvents(
                parseNativeTerminalEventPage(
                    "{\"sessionId\":\"7\",\"events\":[{\"id\":2,\"type\":\"exit\"," +
                        "\"ptyId\":\"5\",\"pid\":123,\"tsMs\":1800,\"sessionId\":\"7\"}]," +
                        "\"hasMore\":false,\"nextCursor\":null}"
                )
            )
            assertEquals(false, panel.loadMoreEventsEnabled(), "no more pages: control disables")
            assertTrue(panel.eventsText().contains("#1 created"), panel.eventsText())
            assertTrue(panel.eventsText().contains("#2 exit"), panel.eventsText())
            // Output snapshot truncation is explicit AND pageable in bounded steps.
            val longOutput = "abcdefgh".repeat(2000)
            panel.setOutput(NativeTerminalOutput("6", longOutput, true))
            assertTrue(
                panel.outputText().contains("showing ${TerminalPanel.OUTPUT_INITIAL_CHARS} of ${longOutput.length} chars"),
                panel.outputText().take(160)
            )
            assertEquals(true, panel.showMoreOutputEnabled(), "truncated output must offer more")
            panel.showMoreOutput()
            assertTrue(
                panel.outputText().contains(longOutput),
                "one bounded step must reveal the whole 16k snapshot"
            )
            assertEquals(false, panel.showMoreOutputEnabled(), "nothing left to reveal")
            // A snapshot beyond the bounded display cap stays honest: the
            // header names the cap and the control disables at the bound.
            val hugeOutput = "z".repeat(100_000)
            panel.setOutput(NativeTerminalOutput("6", hugeOutput, true))
            panel.showMoreOutput()
            panel.showMoreOutput()
            assertTrue(
                panel.outputText().contains("display cap reached"),
                panel.outputText().take(160)
            )
            assertTrue(
                panel.outputText().contains(
                    "showing ${TerminalPanel.OUTPUT_MAX_CHARS} of ${hugeOutput.length} chars"
                ),
                panel.outputText().take(160)
            )
            assertEquals(false, panel.showMoreOutputEnabled(), "local display bound reached")
        }

        // P1: the tree update keeps expansion, selection, detail and scroll.
        step("task tree update preserves expansion, selection and detail by stable node ids") {
            fun findNode(
                tree: javax.swing.JTree,
                panel: TaskTreePanel,
                keyPath: String
            ): DefaultMutableTreeNode? {
                val root = tree.model.root as? DefaultMutableTreeNode ?: return null
                fun walk(node: DefaultMutableTreeNode): DefaultMutableTreeNode? {
                    if (panel.nodeKeyPathForTest(node) == keyPath) return node
                    for (index in 0 until node.childCount) {
                        val found = walk(node.getChildAt(index) as DefaultMutableTreeNode)
                        if (found != null) return found
                    }
                    return null
                }
                return walk(root)
            }

            val task = parseNativeTaskViews(TASK_JSON)[0]
            val model = TaskTree.build(task = task, agents = parseNativeAgents(AGENTS_JSON))
            val panel = TaskTreePanel()
            panel.update(model)
            val tree = panel.treeForTest()
            ParityAwt.layout(panel, 480, 600)

            val sectionPath = "goal/section:children"
            val childPath = "$sectionPath/child:child-1"
            val section = findNode(tree, panel, sectionPath) ?: fail("children section must exist")
            val child = findNode(tree, panel, childPath) ?: fail("child-1 node must exist")
            tree.expandPath(TreePath(section.path))
            tree.selectionPath = TreePath(child.path)
            assertTrue(
                panel.selectedDetailText().contains("child-1"),
                panel.selectedDetailText()
            )
            val viewport = panel.treeScrollForTest().viewport
            viewport.viewPosition = java.awt.Point(0, 24)
            val savedScroll = java.awt.Point(viewport.viewPosition)

            // New data: one more child arrives; nothing was selected/expanded
            // by the operator beyond what we just did.
            val extra = parseNativeAgents(AGENTS_JSON)[1].copy(
                agentId = "child-9",
                sessionId = 44L,
                state = "Running",
                blocker = null,
                result = null
            )
            panel.update(
                TaskTree.build(task = task, agents = parseNativeAgents(AGENTS_JSON) + extra)
            )
            val selectedNode = tree.selectionPath?.lastPathComponent as? DefaultMutableTreeNode
            assertEquals(childPath, selectedNode?.let { panel.nodeKeyPathForTest(it) })
            assertTrue(tree.isExpanded(TreePath(section.path)), "expanded section must stay expanded")
            assertTrue(
                panel.selectedDetailText().contains("child-1"),
                "the inspector detail must follow the preserved selection"
            )
            assertTrue(
                findNode(tree, panel, "$sectionPath/child:child-9") != null,
                "the new child must appear in the same tree"
            )
            assertEquals(savedScroll, java.awt.Point(viewport.viewPosition))

            // A section the operator collapsed must NOT be re-expanded.
            val evidence = findNode(tree, panel, "goal/section:evidence")
                ?: fail("evidence section must exist")
            tree.collapsePath(TreePath(evidence.path))
            panel.update(model)
            assertTrue(
                !tree.isExpanded(TreePath(evidence.path)),
                "a collapsed section must never be re-expanded by an update"
            )
        }

        // P1: an atomic session switch clears message rows and selection too.
        step("evidence navigator session reset clears message rows and every per-session atom") {
            val panel = EvidenceNavigatorPanel()
            panel.setEvidence(listOf(EvidenceRefs.parse("evidence:41")))
            panel.setMessages(
                listOf(NativeMessage(seq = 1, id = 1, role = "user", createdMs = 1, text = "old"))
            )
            panel.selectMessage(0)
            panel.showTranscriptSlice("child old", "old transcript")
            panel.showRetrieval(41L, "old output", 3L, false)
            assertEquals(1, panel.messageCount())
            assertTrue(panel.selectedMessage() != null, "the old row must be selected first")
            panel.resetSessionView()
            assertEquals(0, panel.evidenceCount(), "evidence rows must clear")
            assertEquals(0, panel.messageCount(), "message rows must clear")
            assertEquals(null, panel.selectedMessage(), "message selection must clear")
            assertEquals("", panel.outputText(), "retrieval output must clear")
            assertEquals("", panel.transcriptText(), "child transcript must clear")
            // New-session rows never mix with the previous session's rows.
            panel.setMessages(
                listOf(NativeMessage(seq = 1, id = 2, role = "assistant", createdMs = 2, text = "new"))
            )
            assertEquals(1, panel.messageCount())
            assertEquals("", panel.transcriptText())
            assertEquals("", panel.outputText())
        }

        // P1: real bounded "Load older" on the board instead of an inert hint.
        step("board panel pages older posts through a bounded Load older control") {
            val panel = BoardPanel()
            var requested: Long? = null
            panel.setListener(object : BoardPanel.Listener {
                override fun onRead() {}
                override fun onPost(subject: String, body: String) {}
                override fun onLoadOlder(beforeRevision: Long) {
                    requested = beforeRevision
                }
            })
            panel.setBoard(parseNativeBoardPage(BOARD_PAGE_JSON))
            assertEquals(true, panel.loadOlderEnabled(), "has_more must arm Load older")
            assertEquals(2L, panel.loadOlderCursor())
            assertTrue(panel.headerText().contains("older pages exist"), panel.headerText())
            panel.loadOlder()
            assertEquals(2L, requested)
            panel.appendOlderPage(
                parseNativeBoardPage(
                    "{\"board_id\":7,\"revision\":1,\"posts\":[" +
                        "{\"id\":1,\"board_id\":7,\"author_child\":null,\"author_session\":7," +
                        "\"subject\":\"oldest\",\"body\":\"first post\",\"refs\":[]," +
                        "\"revision\":1,\"created_ms\":1500}" +
                        "],\"next_before_revision\":null,\"has_more\":false}"
                )
            )
            assertEquals(false, panel.loadOlderEnabled(), "no more pages: control disables")
            assertTrue(panel.postsText().contains("Agent 8"), panel.postsText())
            assertTrue(panel.postsText().contains("handoff"), panel.postsText())
            assertTrue(panel.postsText().contains("oldest"), panel.postsText())
            assertTrue(panel.advancedText().contains("revision 1 [root]"), panel.advancedText())
        }

        // The product model: Work (conversation + current run + ONE
        // composer) is the default cluster; Inspect and History are peers.
        // The composer keeps working at 240px because the work plane is the
        // whole content area rather than a fixed-width split half.
        step("Work is the default cluster and the composer is one plane at 240px") {
            val service = FaktorFrontendService(
                Paths.get("unused-binary"),
                Files.createTempDirectory("faktor-work-model-smoke-")
            )
            val panel = FaktorChatPanel(service)
            try {
                assertEquals(listOf("Work", "Inspect", "History"), panel.clusterTitles())
                ParityAwt.layout(panel, 240, 600)
                // Host tab chrome is the only deduction allowed: Aqua (macOS
                // host) insets the tab content by ~21px, Metal (headless CI)
                // by ~5px. A fixed-width plane or a split half (~120px at
                // 240) still fails the bound on every host.
                assertTrue(
                    panel.workPlaneForTest().width >= 240 - 40,
                    "Work must use the full narrow width (host tab chrome aside): " +
                        panel.workPlaneForTest().width
                )
                assertTrue(
                    panel.taskComposerPaneForTest() is javax.swing.JComponent,
                    "the composer pane must be embedded in Work"
                )
                assertTrue(
                    panel.selectPanelForTest("Inspect"),
                    "the Inspect cluster must be reachable: ${panel.tabTitles()}"
                )
                assertTrue(
                    panel.selectPanelForTest("Overview"),
                    "the Overview view must be reachable: ${panel.tabTitles()}"
                )
                assertTrue(panel.selectPanelForTest("Work"), "Work must stay reachable")
                assertTrue(panel.openUtilityForTest("Diagnostics"), "Diagnostics must be reachable")
                assertTrue(panel.utilityOpenForTest(), "the utility view must render")
                panel.closeUtilityForTest()
                assertTrue(!panel.utilityOpenForTest(), "Back must return to the clusters")
                assertTrue(panel.openUtilityForTest("Settings"), "Settings must be reachable")
                panel.closeUtilityForTest()
                assertTrue(panel.openUtilityForTest("Usage"), "Usage must be reachable")
                panel.closeUtilityForTest()
                ParityAwt.layout(panel, 800, 600)
                assertTrue(
                    panel.workPlaneForTest().width >= 800 - 40,
                    "Work must fill the wide content area: " + panel.workPlaneForTest().width
                )
            } finally {
                panel.shutdown()
            }
        }

        step("Task options sheet: bounded scrollable surface, trigger and Back preserve the draft") {
            val service = FaktorFrontendService(
                Paths.get("unused-binary"),
                Files.createTempDirectory("faktor-task-options-smoke-")
            )
            val panel = FaktorChatPanel(service)
            try {
                assertEquals(false, panel.taskOptionsOpenForTest())
                panel.setTaskFieldsForTest("draft goal", "c1")
                panel.setBudgetFieldsForTest("", "")
                assertEquals("Task options ▾", panel.toggleTaskOptionsForTest())
                assertEquals(true, panel.taskOptionsOpenForTest())
                // The sheet body is the scrollable column inside a scroll pane.
                val sheet = panel.taskOptionsSheetForTest() ?: fail("the sheet must exist")
                assertTrue(
                    sheet.components.any { it is javax.swing.JScrollPane },
                    "the sheet must carry a bounded scrollable body"
                )
                assertEquals("draft goal", panel.goalTextForTest())
                assertEquals("Task options ▸", panel.toggleTaskOptionsForTest())
                assertEquals(false, panel.taskOptionsOpenForTest())
                assertEquals("draft goal", panel.goalTextForTest(), "the draft survives the sheet")
                // Escape closes the sheet without starting anything.
                panel.toggleTaskOptionsForTest()
                assertEquals(true, panel.taskOptionsOpenForTest())
                panel.triggerEscapeForTest()
                assertEquals(false, panel.taskOptionsOpenForTest(), "Escape closes the sheet")
                assertEquals("draft goal", panel.goalTextForTest())
                assertEquals(false, panel.askModeSelected(), "Task mode is the default")
                panel.selectModeForTest(ask = true)
                assertEquals(true, panel.askModeSelected())
                panel.selectModeForTest(ask = false)
                assertEquals(false, panel.askModeSelected())
            } finally {
                panel.shutdown()
            }
        }

        if (args.isEmpty()) {
            println("FRONTEND SMOKE PASS (canned only: no daemon binary argument)")
            kotlin.system.exitProcess(if (failures == 0) 0 else 1)
        }
        runAgainstRealDaemon(args[0])

        println(if (failures == 0) "FRONTEND SMOKE PASS" else "FRONTEND SMOKE FAIL ($failures)")
        kotlin.system.exitProcess(if (failures == 0) 0 else 1)
    }

    // ------------------------------------------------------- real daemon

    private fun runAgainstRealDaemon(binaryPath: String) {
        val binary = Paths.get(binaryPath)
        val dataDir = Files.createTempDirectory("faktor-frontend-smoke-")
        // The daemon's provider preflight requires a registered provider: seed
        // the discovered config so the smoke's `default` id is served (an
        // ollama entry is local-only and needs no key).
        Files.write(
            dataDir.resolve("faktor-plus.json"),
            """{"config_version":1,"model":"default","providers":[{"kind":"ollama","id":"default","base_url":"http://127.0.0.1:9","allow_loopback":true}]}"""
                .toByteArray()
        )
        // The shadow mutation default copies the session's WORKSPACE on a
        // task start; a tiny dedicated workspace keeps the smoke hermetic
        // and fast instead of duplicating the whole checkout.
        val workspace = Files.createTempDirectory("faktor-frontend-workspace-")
        Files.write(Paths.get(workspace.toString(), "seed.txt"), "smoke".toByteArray())
        val manager = BackendProcessManager(binary, dataDir)
        var connection: BackendConnection? = null
        try {
            var sessionId: String? = null
            step("start daemon for frontend smoke") {
                connection = manager.start()
                println("  port=${connection!!.port} pid=${connection!!.pid()}")
            }
            val conn = connection
            if (conn != null) {
                val client = NativeClient.forConnection(conn)
                step("native health + readiness") {
                    if (!client.health().ok) fail("health ok=false")
                    if (!client.awaitReady(10_000L).ready) fail("daemon never ready")
                }
                step("real: /models advertises the attachment contract the client consumes") {
                    val catalog = client.modelCatalog()
                    // This smoke daemon boots with the baked default config
                    // (no configured providers), so the catalog may be
                    // empty; the advertisement itself is pinned by the
                    // shared fixture + canned parse rows above and by the
                    // Rust route test. When a provider IS registered, the
                    // served contract must parse and drive the client.
                    if (catalog.isEmpty()) {
                        println("  (no configured providers: /models is empty; contract covered by fixture rows)")
                        return@step
                    }
                    val entry = catalog.first()
                    val limits = entry.attachmentLimits
                        ?: fail("the real daemon must advertise attachmentLimits")
                    assertEquals(
                        entry.documentCapable,
                        limits.document.capable,
                        "the document gate must match documentCapable"
                    )
                    assertTrue(limits.maxUploadBytes > 0, "upload ceiling must be positive")
                    assertTrue(
                        limits.maxRequestBytes >= limits.maxUploadBytes,
                        "the request ceiling can never sit below the upload ceiling"
                    )
                    assertTrue(limits.image.mimes.isNotEmpty(), "image mimes must be advertised")
                    assertTrue(limits.document.mimes.isNotEmpty(), "document mimes must be advertised")
                    assertTrue(
                        limits.image.mimes.all { it.maxBytes > 0 },
                        "every advertised per-MIME bound is positive"
                    )
                    // The client policy consumes the advertised numbers for
                    // the exact (provider, model) pair; the emergency ceiling
                    // remains the fallback for unknown models.
                    val policy = AttachmentImages.policyForModel(catalog, entry.provider, entry.model)
                    assertEquals("advertised", policy.source)
                    assertEquals(limits.maxUploadBytes, policy.maxUploadBytes)
                    assertEquals(limits.image.mimes.map { it.mime }, policy.imageMimes)
                    assertTrue(
                        AttachmentImages.emergencyPolicy().maxImageBytes <=
                            (limits.image.mimes.map { it.maxBytes }.max() ?: 0),
                        "the emergency ceiling must stay conservative"
                    )
                    assertEquals(
                        "emergency",
                        AttachmentImages.policyForModel(catalog, entry.provider, "unknown-model").source
                    )
                }
                step("create session") {
                    sessionId = client.createSession(
                        "default", "default", workspace.toString(), "frontend smoke"
                    ).id
                }
                val sid = sessionId
                if (sid != null) {
                    step("task-run accepts the planned workspace-relative `files` (real daemon)") {
                        // The full client path: pick an absolute file inside the
                        // workspace, let the attachment panel relativize it,
                        // plan it, and post the planned `files` — the daemon
                        // must ACCEPT the relative list (absolute paths are
                        // deterministically refused server-side). A source
                        // file (not a deliverable document) stays a path.
                        val attachmentRelative = "seed-attachment.rs"
                        Files.write(
                            Paths.get(workspace.toString(), attachmentRelative),
                            "fn main() {}".toByteArray()
                        )
                        val panel = AttachmentsPanel(workspace)
                        panel.addFiles(
                            listOf(Paths.get(workspace.toString(), attachmentRelative).toString())
                        )
                        assertEquals(
                            listOf(attachmentRelative),
                            panel.files(),
                            "the picked absolute path must relativize to the workspace root"
                        )
                        assertEquals(0, panel.refusalCount())
                        val policy = AttachmentImages.emergencyPolicy()
                        val plan = planAttachments(panel.files(), emptyList(), policy, workspace)
                        assertEquals(listOf(attachmentRelative), plan.pathFiles)
                        assertEquals(0, plan.uploads.size)
                        val started = client.startTaskRun(
                            sid,
                            "frontend smoke attachment",
                            files = plan.pathFiles,
                            submissionId = java.util.UUID.randomUUID().toString()
                        )
                        if (started.runId.isEmpty()) fail("no run id")
                        println("  run=${started.runId} state=${started.state}")
                    }
                    step("binary attachment upload keeps ref_id and serves ref-addressed metadata/bytes") {
                        val payload = "smoke bytes".toByteArray()
                        val uploaded = client.uploadAttachment(
                            sid,
                            "text/plain",
                            "smoke.txt",
                            java.util.Base64.getEncoder().encodeToString(payload)
                        )
                        if (uploaded.refId < 1L) fail("the upload must carry a ref_id")
                        assertEquals("text/plain", uploaded.mime)
                        assertEquals(payload.size.toLong(), uploaded.size)
                        assertEquals(
                            uploaded,
                            client.attachmentReference(sid, uploaded.refId),
                            "the ref route resolves exactly THIS reference"
                        )
                        val refBytes = client.attachmentReferenceBytes(sid, uploaded.refId)
                        assertEquals("text/plain", refBytes.mime, "ref bytes carry THAT reference MIME")
                        assertTrue(payload.contentEquals(refBytes.bytes), "ref bytes are byte-exact")
                        val blobBytes = client.attachmentBlobBytes(sid, uploaded.digest)
                        assertTrue(
                            payload.contentEquals(blobBytes),
                            "blob bytes are the raw CAS bytes (no reference MIME invented)"
                        )
                        println("  ref=${uploaded.refId} digest=${uploaded.digest.take(12)}...")
                    }
                    step("permission list is served (reply route reachable)") {
                        val permissions = client.permissions(sid)
                        println("  pending permissions=${permissions.size}")
                    }
                    step("unknown permission id is a typed 409 conflict (never a blind retry)") {
                        try {
                            client.replyPermission(sid, "999999", "allow")
                            fail("an unknown permission id must not answer 200")
                        } catch (e: NativeApiException) {
                            if (e.status != 409 || e.code != "conflict" || e.retryable) {
                                fail("unexpected permission refusal ${e.status} ${e.code}")
                            }
                            assertTrue(
                                NativePermissionReplyRefusal.isUnknownOrResolved(e),
                                "the conflict must classify as unknown/resolved"
                            )
                        }
                    }
                    step("unknown tournament is a typed 404") {
                        try {
                            client.tournamentState(sid, "does-not-exist")
                            fail("unknown tournament must not answer 200")
                        } catch (e: NativeApiException) {
                            if (e.status != 404) {
                                fail("unexpected tournament error ${e.status} ${e.code}")
                            }
                        }
                    }
                    step("tournament listing + decide/abort are typed") {
                        val summaries = client.tournaments(sid)
                        println("  tournaments=${summaries.size}")
                        try {
                            client.decideTournament(sid, "does-not-exist")
                            fail("unknown decide must not answer 200")
                        } catch (e: NativeApiException) {
                            if (e.status != 404) {
                                fail("unexpected decide error ${e.status} ${e.code}")
                            }
                        }
                        try {
                            client.abortTournament(sid, "does-not-exist", "smoke")
                            fail("unknown abort must not answer 200")
                        } catch (e: NativeApiException) {
                            if (e.status != 404) {
                                fail("unexpected abort error ${e.status} ${e.code}")
                            }
                        }
                    }
                    step("board GET/POST round-trips on the real daemon") {
                        val page = client.board(sid, limit = 10L)
                        if (page.boardId <= 0L) fail("board id must be positive")
                        println("  board rev=${page.revision} posts=${page.posts.size}")
                        val post = client.boardPost(
                            sid, "smoke subject", "smoke body", listOf("evidence:1")
                        )
                        if (post.revision <= 0L) fail("post revision must be positive")
                        val after = client.board(sid, limit = 10L)
                        val seen = after.posts.any {
                            it.revision == post.revision && it.subject == "smoke subject"
                        }
                        if (!seen) fail("the posted subject must appear on the next page")
                    }
                    step("hostile board post is a typed 4xx (never a silent write)") {
                        try {
                            client.boardPost(sid, "", "body")
                            fail("empty subject must not answer 201")
                        } catch (e: NativeApiException) {
                            if (e.status !in 400..499) {
                                fail("unexpected board error ${e.status} ${e.code}")
                            }
                        }
                    }
                    step("presentation transition on an unknown child is a typed 404") {
                        try {
                            client.setAgentPresentation(sid, "no-such-child", "background")
                            fail("unknown child presentation must not answer 200")
                        } catch (e: NativeApiException) {
                            if (e.status != 404) {
                                fail("unexpected presentation error ${e.status} ${e.code}")
                            }
                        }
                    }
                    step("orchestrator graph is typed (graph | 404 | 409)") {
                        try {
                            val graph = client.orchestratorGraph(sid)
                            println("  graph plan=${graph.planId} children=${graph.children.size}")
                        } catch (e: NativeApiException) {
                            if (e.status != 404 && e.status != 409) {
                                fail("unexpected graph error ${e.status} ${e.code}")
                            }
                        }
                    }
                    step("evidence access stays typed (404 unknown / 503 unwired)") {
                        try {
                            client.evidence(sid, 1L)
                            fail("unknown evidence id must not answer 200")
                        } catch (e: NativeApiException) {
                            if (e.status != 404 && e.status != 503) {
                                fail("unexpected evidence error ${e.status} ${e.code}")
                            }
                        }
                    }
                    step("outcomes ledger reads with strict bounds (bounds never fabricated)") {
                       val page = client.outcomes(limit = 5L)
                        println(
                            "  outcomes runs=${page.metrics.runs} " +
                                "verified=${page.metrics.verifiedChanges} " +
                                "truncated=${page.metrics.truncated}"
                        )
                        if (page.nextCursor != null && page.runs.isEmpty()) {
                            fail("a non-null cursor with no rows is contradictory")
                        }
                        try {
                            client.outcomes(limit = 0L)
                            fail("a zero page limit must be refused")
                        } catch (e: NativeApiException) {
                            if (e.status !in 400..499) {
                                fail("unexpected outcomes refusal ${e.status} ${e.code}")
                            }
                        }
                    }
                }
           }
        } finally {
            if (connection != null) {
                val c = connection!!
                try {
                    manager.stop(c)
                    println("PASS stop daemon")
                } catch (e: Throwable) {
                    failures++
                    println("FAIL stop daemon: ${e.message}")
                    c.process.destroyForcibly()
                }
            }
            dataDir.toFile().deleteRecursively()
            workspace.toFile().deleteRecursively()
        }
    }

    // ------------------------------------------------- I03 change review

    /**
     * I03: rows derive from SERVED relationships only; the immutable basis
     * resolves in the exact order (candidate source diff -> attached refs ->
     * explicit unavailable); the host seam receives bytes and never a
     * workspace file; navigation is bounded; a read failure clears rows.
     */
    private fun changeReviewUnitTests() {
        val proof = dev.faktor.shared.NativeCandidateProof(
            taskRevision = "rev-2",
            baseManifestHash = "base-manifest",
            candidateManifestHash = "cand-manifest",
            sourceDiffEvidence = "evidence:77",
            riskReportEvidence = null,
            accountingSnapshotDigest = null,
            runId = "run-1",
            runBaseSnapshot = "tm1:base",
            candidateSnapshot = "tm1:cand",
            sourcesDigest = "src-digest",
            changedFilesDigest = "cf-digest"
        )
        val record1 = dev.faktor.shared.NativeVerificationRecord(
            recordId = "r1",
            status = "passed",
            criteriaPassed = 1,
            criteriaTotal = 1,
            checks = listOf("unit=passed"),
            changedFiles = listOf("src/a.rs"),
            criteria = listOf(
                dev.faktor.shared.NativeCriterionVerdict(
                    criterionKey = "c1",
                    passed = true,
                    evidence = "evidence:41;file:src/a.rs",
                    binding = dev.faktor.shared.NativeCriterionBinding(
                        kind = "file_state",
                        path = "src/a.rs",
                        expectedDigest = "sha256:aa"
                    ),
                    origin = "user",
                    requirement = "required",
                    verdict = "pass"
                )
            ),
            candidateProof = proof,
            completedMs = 100L
        )
        val record2 = dev.faktor.shared.NativeVerificationRecord(
            recordId = "r2",
            status = "failed",
            criteriaPassed = 0,
            criteriaTotal = 1,
            checks = emptyList(),
            changedFiles = listOf("src/b.kt"),
            criteria = listOf(
                dev.faktor.shared.NativeCriterionVerdict(
                    criterionKey = "c2",
                    passed = false,
                    evidence = "evidence:42",
                    binding = dev.faktor.shared.NativeCriterionBinding(
                        kind = "evidence",
                        evidenceId = "42"
                    ),
                    origin = "user",
                    requirement = "required",
                    verdict = "fail"
                )
            ),
            completedMs = 50L
        )
        val verification = dev.faktor.shared.NativeTaskVerification(
            sessionId = "s1",
            taskId = "t1",
            records = listOf(record1, record2)
        )
        val task = parseNativeTaskViews(TASK_JSON)[0].copy(changedFiles = listOf("src/a.rs", "src/b.kt"))

        step("I03 rows derive only served relationships (path + kind + attached refs)") {
            val model = changeReviewOf(projection = null, task = task, taskVerification = verification)
            assertEquals(listOf("src/a.rs", "src/b.kt"), model.rows.map { it.path })
            val a = model.rows[0]
            assertEquals(null, a.changeKind, "no daemon serves a change kind; none is fabricated")
            assertEquals(listOf("r1"), a.verificationRecordIds)
            assertEquals(listOf("c1"), a.criterionKeys)
            assertEquals(listOf(41L), a.evidenceRefs.mapNotNull { it.id })
            assertEquals("sha256:aa", a.expectedDigest)
            val b = model.rows[1]
            assertEquals(listOf("r2"), b.verificationRecordIds)
            assertTrue(b.criterionKeys.isEmpty(), "an evidence binding is not a path relationship")
            assertTrue(b.evidenceRefs.isEmpty(), "no served ref is attached to src/b.kt")
            // The projection's filesChanged is part of the served union.
            val projection = dev.faktor.shared.NativeProjection(
                sessionId = "s1", title = "T", provider = "p", model = "m",
                lifecycle = "open", machine = "ready", label = "ready", active = true,
                terminal = false, activeModel = null, activeTool = null,
                filesChanged = listOf("src/c.py"), verification = emptyList(), queued = 0
            )
            val union = changeReviewOf(projection, task, verification)
            assertEquals(listOf("src/a.rs", "src/b.kt", "src/c.py"), union.rows.map { it.path })
            // The candidate display is the served identity; raw hashes stay
            // advanced detail.
            assertEquals("candidate tm1:cand", union.basis?.candidateLabel)
            assertEquals(77L, union.basis?.sourceDiffEvidenceId)
            assertTrue(
                union.basis?.candidateDetail?.any { it.contains("candidate manifest: cand-manifest") } == true,
                "raw hashes stay available as advanced detail"
            )
        }

        step("I03 basis resolution follows the exact order a -> b -> c") {
            val model = changeReviewOf(null, task, verification)
            val first = resolveChangeBasis(model, 0)
            assertTrue(first is BasisResolution.Single, "the candidate source diff wins first")
            assertEquals(77L, (first as BasisResolution.Single).evidenceId)
            assertEquals(
                "candidate tm1:cand — source diff — evidence 77 (immutable)",
                first.title
            )
            // Without a served source diff, the selected path's refs resolve.
            val basisless = model.copy(basis = model.basis?.copy(sourceDiffEvidenceRaw = null, sourceDiffEvidenceId = null))
            val second = resolveChangeBasis(basisless, 0)
            assertEquals(41L, (second as BasisResolution.Single).evidenceId)
            assertEquals("src/a.rs — evidence 41 (immutable)", second.title)
            // Two immutable artifacts for the same path: a true diff is allowed.
            val pairRow = model.rows[0].copy(
                evidenceRefs = listOf(EvidenceRef(41L, "evidence:41"), EvidenceRef(42L, "evidence:42"))
            )
            val pair = resolveChangeBasis(basisless.copy(rows = listOf(pairRow)), 0)
            assertTrue(pair is BasisResolution.Pair, "two served artifacts for one path form a diff")
            assertEquals(41L, (pair as BasisResolution.Pair).first)
            assertEquals(42L, pair.second)
            assertTrue(pair.title.contains("\u2194"), pair.title)
            // No basis and no refs: the explicit in-panel state.
            val none = basisless.copy(rows = listOf(pairRow.copy(evidenceRefs = emptyList())))
            assertTrue(resolveChangeBasis(none, 0) is BasisResolution.Unavailable)
            // A served-but-invalid id is a typed refusal, never skipped.
            val invalid = model.copy(
                basis = model.basis?.copy(
                    sourceDiffEvidenceRaw = "not-an-id",
                    sourceDiffEvidenceId = null
                )
            )
            val refused = resolveChangeBasis(invalid, 0)
            assertTrue(refused is BasisResolution.Invalid, "an invalid served id refuses typed")
            assertTrue(
                (refused as BasisResolution.Invalid).reason.contains("not a valid evidence id"),
                refused.reason
            )
        }

        step("I03 adapter seam opens the served bytes exactly (unicode + truncated header)") {
            val unicode = "caf\u00e9 \u2014 \u00fcn\u00efcode \u2713"
            val hit = java.util.concurrent.atomic.AtomicInteger(0)
            val host = RecordingChangeReviewHost()
            val controller = ChangeReviewController(host) { id ->
                hit.incrementAndGet()
                NativeEvidenceRetrieval(
                    id, "{\"selector\":\"all\"}",
                    unicode.toByteArray(Charsets.UTF_8),
                    unicode.toByteArray(Charsets.UTF_8).size.toLong(),
                    true
                )
            }
            val model = changeReviewOf(null, task, verification)
            assertEquals(ChangeOpenResult.Opened, controller.open(model, 0))
            assertEquals(1, hit.get())
            assertEquals(1, host.artifacts.size)
            val artifact = host.artifacts[0]
            assertEquals(77L, artifact.evidenceId)
            assertEquals(true, artifact.truncatedByPolicy)
            val lines = artifact.text.split('\n')
            assertEquals(TRUNCATED_BY_POLICY_HEADER, lines.first(), "truncated first line")
            assertTrue(artifact.text.contains(unicode), "the served bytes are byte-exact (unicode)")
        }

        step("I03 large bounded artifact opens byte-exact") {
            val bytes = ByteArray(1 shl 20) { 'x'.code.toByte() }
            val host = RecordingChangeReviewHost()
            val controller = ChangeReviewController(host) { id ->
                NativeEvidenceRetrieval(id, "{}", bytes, bytes.size.toLong(), false)
            }
            val model = changeReviewOf(null, task, verification)
            assertEquals(ChangeOpenResult.Opened, controller.open(model, 0))
            assertEquals(String(bytes, Charsets.UTF_8), host.artifacts[0].text)
            assertEquals(false, host.artifacts[0].truncatedByPolicy)
        }

        step("I03 invalid/unknown/foreign evidence refused typed and nothing opens") {
            val host = RecordingChangeReviewHost()
            val unknown = ChangeReviewController(host) { id ->
                throw NativeApiException(404, "not_found", "no evidence $id", false)
            }
            val model = changeReviewOf(null, task, verification)
            val refusedUnknown = unknown.open(model, 0)
            assertTrue(refusedUnknown is ChangeOpenResult.Refused, "unknown evidence refuses")
            assertTrue(
                (refusedUnknown as ChangeOpenResult.Refused).reason.contains("status 404"),
                refusedUnknown.reason
            )
            val foreign = ChangeReviewController(host) { id ->
                NativeEvidenceRetrieval(999L, "{}", "other".toByteArray(), 5L, false)
            }
            val refusedForeign = foreign.open(model, 0)
            assertTrue(refusedForeign is ChangeOpenResult.Refused, "foreign evidence refuses")
            assertTrue(
                (refusedForeign as ChangeOpenResult.Refused).reason.contains("foreign artifact"),
                refusedForeign.reason
            )
            val invalid = ChangeReviewController(host) { id ->
                fail("an invalid id must never be retrieved (asked #$id)")
            }
            val invalidModel = model.copy(
                basis = model.basis?.copy(sourceDiffEvidenceRaw = "nope", sourceDiffEvidenceId = null)
            )
            assertTrue(invalid.open(invalidModel, 0) is ChangeOpenResult.Refused)
            assertEquals(0, host.artifacts.size, "nothing opens on any refusal")
            assertEquals(0, host.diffs.size)
        }

        step("I03 no workspace file path is ever opened (adapter seam)") {
            val host = RecordingChangeReviewHost()
            val controller = ChangeReviewController(host) { id ->
                NativeEvidenceRetrieval(id, "{}", "content".toByteArray(), 7L, false)
            }
            // No served candidate source diff here: resolution must fall to
            // the selected path's attached evidence ref. The row path is a
            // workspace-looking absolute path: it may ONLY ever appear as the
            // immutable title, never as an open target.
            val model = ChangeReviewModel(
                basis = null,
                rows = listOf(
                    ChangeRow(
                        path = "/etc/passwd",
                        changeKind = null,
                        verificationRecordIds = emptyList(),
                        criterionKeys = emptyList(),
                        evidenceRefs = listOf(EvidenceRef(41L, "evidence:41")),
                        expectedDigest = null
                    )
                )
            )
            assertEquals(ChangeOpenResult.Opened, controller.open(model, 0))
            assertEquals(1, host.artifacts.size)
            assertEquals(
                "/etc/passwd — evidence 41 (immutable)",
                host.artifacts[0].title,
                "the path appears only in the immutable title, never as an open target"
            )
            assertEquals(0, host.diffs.size)
            // Structural: the seam carries no file/Path parameter at all.
            for (method in ChangeReviewHost::class.java.methods) {
                for (parameter in method.parameterTypes) {
                    assertTrue(
                        parameter != java.io.File::class.java &&
                            parameter != java.nio.file.Path::class.java,
                        "the host seam must not take file paths: ${method.name}"
                    )
                }
            }
        }

        step("I03 diff-unavailable state carries the exact copy; no apply/rebase control") {
            assertEquals("Diff unavailable", DIFF_UNAVAILABLE_LABEL)
            assertEquals(
                "The runtime does not serve an immutable candidate content read for this change.",
                DIFF_UNAVAILABLE_COPY
            )
            val panel = ChangesPanel()
            panel.update(changeReviewOf(null, task, verification))
            panel.showDiffUnavailable()
            assertEquals(true, panel.diffUnavailableVisible())
            assertEquals(DIFF_UNAVAILABLE_LABEL, panel.diffUnavailableLabelText())
            assertEquals(DIFF_UNAVAILABLE_COPY, panel.diffUnavailableCopyText())
            val controls = ArrayList<String>()
            collectButtonTexts(panel, controls)
            for (label in controls) {
                assertTrue(
                    !label.contains("Rebase", ignoreCase = true) &&
                        !label.contains("Apply", ignoreCase = true),
                    "no rebase/apply capability exists; got control \"$label\""
                )
            }
        }

        step("I03 previous/next are bounded and selection survives refreshes") {
            val panel = ChangesPanel()
            val rows = listOf("a.rs", "b.rs", "c.rs").map {
                ChangeRow(it, null, emptyList(), emptyList(), emptyList(), null)
            }
            panel.update(ChangeReviewModel(basis = null, rows = rows))
            assertEquals(0, panel.selectedIndex())
            assertEquals(false, panel.previousEnabled())
            assertEquals(true, panel.nextEnabled())
            panel.clickNext()
            panel.clickNext()
            assertEquals(2, panel.selectedIndex())
            assertEquals(false, panel.nextEnabled(), "next is disabled at the end")
            panel.clickNext()
            assertEquals(2, panel.selectedIndex(), "next past the end stays bounded")
            // A refresh reordered/replaced rows: the same path stays selected.
            panel.selectIndex(1)
            assertEquals("b.rs", panel.selectedPath())
            panel.update(
                ChangeReviewModel(basis = null, rows = listOf(rows[2], rows[1], rows[0]))
            )
            assertEquals("b.rs", panel.selectedPath(), "selection survives a refresh")
            assertEquals(1, panel.selectedIndex())
            // Shrinking the row set clamps the index.
            panel.update(ChangeReviewModel(basis = null, rows = listOf(rows[0])))
            assertEquals(0, panel.selectedIndex())
            // A read failure clears every row and states the reason.
            panel.setUnavailable("change read failed: 503")
            assertEquals(0, panel.rowCount(), "no stale rows behind a read failure")
            assertEquals("change read failed: 503", panel.unavailableReason())
            assertEquals(CHANGES_UNAVAILABLE_LABEL, panel.unavailableHeadlineText())
            // The empty served state uses the exact I03 copy.
            panel.update(ChangeReviewModel(basis = null, rows = emptyList()))
            assertEquals(CHANGES_EMPTY_COPY, panel.emptyText())
        }

        step("I03 recheck re-derives conflict facts (no rebase control)") {
            val model = changeReviewOf(null, task, verification)
            val unchanged = basisConflictFacts(model, model)
            assertEquals(1, unchanged.size)
            assertTrue(unchanged[0].contains("unchanged"), unchanged[0])
            val moved = model.copy(
                basis = model.basis?.copy(sourceDiffEvidenceRaw = "evidence:78", sourceDiffEvidenceId = 78L)
            )
            val facts = basisConflictFacts(model, moved)
            assertTrue(facts.any { it.contains("conflict") && it.contains("#77") && it.contains("#78") }, facts.toString())
            val shrunken = model.copy(rows = model.rows.take(1))
            assertTrue(basisConflictFacts(model, shrunken).any { it.contains("changed-file set changed") })
            assertTrue(
                basisConflictFacts(model, changeReviewOf(null, null, null, "read failed"))
                    .any { it.contains("could not be re-read") }
            )
        }
    }

    // --------------------------------------------------------- I07 outcomes

    private const val OUTCOMES_METRICS_JSON = "{" +
        "\"runs\":10,\"verifiedChanges\":2,\"verifiedFirstVerificationPassChanges\":1," +
        "\"verifiedFirstVerificationPassPpm\":500000," +
        "\"moneyCompleteRuns\":10,\"moneyPendingRuns\":0,\"moneyUnavailableRuns\":0," +
        "\"moneyIncompleteExcluded\":0,\"verifiedChangesMoneyComplete\":2," +
        "\"totalVerifiedSpendMicro\":\"3000000\",\"successfulRunCostMicro\":\"1500000\"," +
        "\"totalSpendMicro\":\"90000000\"," +
        "\"verifiedCostPerSuccessfulChangeMicro\":\"45000000\"," +
        "\"totalReworkSpendMicro\":\"1000000\",\"reworkCostRatioPpm\":11111," +
        "\"localRuns\":4,\"localOnlyRuns\":3,\"localOnlyVerifiedChanges\":1," +
        "\"localOnlyVerifiedPpm\":333333," +
        "\"localFinalRuns\":2,\"localFinalVerifiedChanges\":1,\"localFinalVerifiedPpm\":500000," +
        "\"localFirstRuns\":5,\"localFirstSuccessChanges\":1,\"localFirstSuccessPpm\":200000," +
        "\"localToHostedFallbackRuns\":2,\"localToHostedFallbackPpm\":400000," +
        "\"localClassedRuns\":10,\"localMeasurement\":\"measured\"," +
        "\"hostedOnlyRuns\":6,\"runsWithSteps\":10,\"hostedOnlyPpm\":600000," +
        "\"avoidedHostedInferenceMicro\":null,\"avoidedHostedInferenceMeasurement\":\"unavailable\"," +
        "\"avoidedHostedMeasuredRuns\":0," +
        "\"attributionCompleteRuns\":10,\"attributionPartialRuns\":0," +
        "\"attributionUnavailableRuns\":0,\"attributionIncompleteExcluded\":0," +
        "\"totalWallTimeMs\":100," +
        "\"meanTimeToVerifiedChangeMs\":50," +
        "\"humanInterventionRuns\":1,\"humanInterventionPpm\":100000," +
        "\"rollbackRuns\":1,\"regressionRollbackPpm\":100000," +
        "\"checksPassed\":3,\"checksRequired\":4,\"proofCoveragePpm\":750000," +
        "\"frontierEscalationRuns\":0,\"frontierClassedRuns\":10," +
        "\"frontierEscalationClassedRuns\":0,\"frontierEscalationPpm\":null," +
        "\"frontierEscalationMeasurement\":\"unmeasured\"," +
        "\"scanned\":10,\"truncated\":false}"

    private const val OUTCOMES_PAGE_JSON = "{\"metrics\":" + OUTCOMES_METRICS_JSON + "," +
        "\"runs\":[{\"runId\":\"run-9\",\"sessionId\":1,\"taskId\":2," +
        "\"taskClass\":\"Implementation\",\"riskClass\":\"low\",\"candidateSnapshot\":null," +
        "\"finalSnapshot\":\"tm1:x\",\"accepted\":true,\"integrated\":true,\"verified\":true," +
        "\"verifiedFirstVerificationPass\":true,\"repairAttempts\":0," +
        "\"modelSequence\":[{\"provider\":\"local\",\"model\":\"m\",\"local\":true," +
        "\"legacyLocal\":false,\"executionClass\":\"local\",\"spendMicro\":\"1\"}]," +
        "\"firstExecutionClass\":\"local\",\"finalExecutionClass\":\"local\"," +
        "\"attributionState\":\"complete\"," +
        "\"moneyState\":\"complete\"," +
        "\"modelSpendMicro\":\"1\",\"verificationSpendMicro\":\"0\",\"reworkSpendMicro\":\"0\"," +
        "\"avoidedHostedMicro\":null,\"avoidedHostedMeasurement\":\"unavailable\"," +
        "\"wallTimeMs\":5,\"humanInterventions\":0," +
        "\"proof\":{\"checksPassed\":1,\"checksRequired\":1,\"coveragePpm\":1000000}," +
        "\"changeScale\":{\"files\":1,\"addedLines\":2,\"removedLines\":3}," +
        "\"rollbackEvents\":0,\"recoveryEvents\":0,\"finalBasis\":\"deterministic_checks\"," +
        "\"frontierEscalations\":0,\"completedAtMs\":42}]," +
        "\"nextCursor\":{\"completedAtMs\":42,\"runId\":\"run-9\"}}"

    /** The caller organization's derived value-entitlement report. */
    private const val VALUE_ENTITLEMENTS_JSON = "{\"ok\":true,\"value_entitlements\":{" +
        "\"organization_id\":\"org-local\",\"plan_id\":\"pro\",\"plan_found\":true," +
        "\"subscription_active\":true," +
        "\"plan_entitlements\":[\"outcome_analytics\",\"shared_intelligence\"]," +
        "\"governance\":{\"sso\":false,\"scim\":false},\"decisions\":[" +
        "{\"entitlement\":\"outcome_analytics\",\"hook\":\"outcome_ledger\",\"granted\":true}," +
        "{\"entitlement\":\"cross_repo_intelligence\",\"hook\":\"cross_repo_index\"," +
        "\"granted\":false,\"cause\":\"not_in_plan\"}]," +
        "\"now_ms\":1700000000000}}"

    /**
     * I07: exact metric names/definitions, the served counts and denominators,
     * the empty-denominator copy, scan truncation, the unmeasured
     * counterfactual and the T29 read-failure state (no monetary value).
     */
    private fun outcomesUnitTests() {
        step("I07 parse validity: additive fields ignored, malformed typed, exact money") {
            val withUnknown = OUTCOMES_METRICS_JSON.replace(
                "\"scanned\":10",
                "\"scanned\":10,\"futureMetric\":123"
            )
            val tolerant = parseProtocolOutcomesPage(
                JsonCodec.parse(
                    "{\"metrics\":$withUnknown,\"runs\":[],\"nextCursor\":null}"
                ).view("test")
            )
            assertEquals(10L, tolerant.metrics.runs)
            assertEquals(false, tolerant.metrics.truncated)
            val malformed = "{\"metrics\":" +
                OUTCOMES_METRICS_JSON.replace("\"runs\":10", "\"runs\":\"ten\"") +
                ",\"runs\":[],\"nextCursor\":null}"
            try {
                parseProtocolOutcomesPage(JsonCodec.parse(malformed).view("test"))
                fail("a string runs counter must fail typed")
            } catch (e: NativeProtocolException) {
                assertTrue(e.message!!.contains("runs"), e.message!!)
            }
            val page = parseProtocolOutcomesPage(JsonCodec.parse(OUTCOMES_PAGE_JSON).view("test"))
            assertEquals(2L, page.metrics.verifiedChanges)
            assertEquals("45000000", page.metrics.verifiedCostPerSuccessfulChangeMicro)
            assertEquals("run-9", page.runs[0].runId)
            assertEquals(42L, page.nextCursor?.completedAtMs)
        }

        step("I07 exact metric names + Successful-Run Cost vs VCSC + local cohorts stay distinct") {
            val page = parseProtocolOutcomesPage(JsonCodec.parse(OUTCOMES_PAGE_JSON).view("test"))
            val model = outcomesPanelModelOf(page)
            assertEquals("ok", model.state)
            val names = model.metricNames()
            for (name in listOf(
                "Successful-Run Cost",
                "Verified Cost per Successful Change (VCSC)",
                "Verified First Verification Pass Rate",
                "Rework Cost Ratio",
                "Avoided Hosted Inference",
                "Mean Time to Verified Change",
                "Human Intervention Rate",
                "Regression/Rollback Rate",
                "Proof Coverage",
                "Frontier Escalation Rate",
                "Local-Only Verified Rate",
                "Local-Final Verified Rate",
                "Local-First Success Rate",
                "Local-to-Hosted Fallback Rate",
                "Hosted-Only Rate"
            )) {
                assertTrue(names.contains(name), "metric \"$name\" must render")
            }
            assertEquals("$1.5000 \u00b7 1500000\u00b5\$", model.metricValue("Successful-Run Cost"))
            assertEquals("$45.0000 \u00b7 45000000\u00b5\$", model.metricValue("Verified Cost per Successful Change (VCSC)"))
            assertEquals("50.0000%", model.metricValue("Verified First Verification Pass Rate"))
            assertEquals("33.3333%", model.metricValue("Local-Only Verified Rate"))
            assertEquals("50.0000%", model.metricValue("Local-Final Verified Rate"))
            assertEquals("20.0000%", model.metricValue("Local-First Success Rate"))
            assertEquals("40.0000%", model.metricValue("Local-to-Hosted Fallback Rate"))
            assertEquals("60.0000%", model.metricValue("Hosted-Only Rate"))
            assertEquals("1.1111%", model.metricValue("Rework Cost Ratio"))
            assertEquals("75.0000%", model.metricValue("Proof Coverage"))
            // The VCSC explanation names the audit example (failures included).
            val vcsc = model.metricRows.first { it.name == "Verified Cost per Successful Change (VCSC)" }
            assertTrue(vcsc.explanation.contains("$91 per successful change"), vcsc.explanation)
        }

        step("I07 empty verified denominator uses the exact copy; zero avoided is not measured") {
            val page = parseProtocolOutcomesPage(JsonCodec.parse(OUTCOMES_PAGE_JSON).view("test"))
            val empty = outcomesPanelModelOf(
                page.copy(metrics = page.metrics.copy(verifiedChanges = 0))
            )
            assertEquals(OUTCOMES_NO_VERIFIED_CHANGES, empty.metricValue("Successful-Run Cost"))
            assertEquals(OUTCOMES_NO_VERIFIED_CHANGES, empty.metricValue("Verified Cost per Successful Change (VCSC)"))
            assertEquals(OUTCOMES_NO_VERIFIED_CHANGES, empty.metricValue("Verified First Verification Pass Rate"))
            assertEquals(OUTCOMES_NO_VERIFIED_CHANGES, empty.metricValue("Mean Time to Verified Change"))
            // An UNAVAILABLE counterfactual is unmeasured, never a free claim.
            assertEquals("not measured", empty.metricValue("Avoided Hosted Inference"))
            val measured = outcomesPanelModelOf(
                page.copy(
                    metrics = page.metrics.copy(
                        avoidedHostedInferenceMicro = "1234567890",
                        avoidedHostedInferenceMeasurement = "measured"
                    )
                )
            )
            val avoided = measured.metricValue("Avoided Hosted Inference")!!
            assertTrue(avoided.contains("1234567890"), avoided)
            assertTrue(!avoided.contains("E"), "money never renders scientific notation: $avoided")
            // A MEASURED zero stays a measured zero (zero is only measured).
            val measuredZero = outcomesPanelModelOf(
                page.copy(
                    metrics = page.metrics.copy(
                        avoidedHostedInferenceMicro = "0",
                        avoidedHostedInferenceMeasurement = "measured"
                    )
                )
            )
            val zero = measuredZero.metricValue("Avoided Hosted Inference")!!
            assertTrue(zero.contains("0"), zero)
            assertTrue(!zero.contains("not measured"), zero)
        }

        step("I07 truncation is shown explicitly and big money renders exactly") {
            val page = parseProtocolOutcomesPage(JsonCodec.parse(OUTCOMES_PAGE_JSON).view("test"))
            val truncated = outcomesPanelModelOf(
                page.copy(metrics = page.metrics.copy(truncated = true, scanned = 10000))
            )
            assertEquals(true, truncated.truncated)
            val panel = OutcomesPanel()
            panel.setModel(truncated)
            assertTrue(panel.truncatedText().contains("[TRUNCATED]"), panel.truncatedText())
            assertTrue(panel.truncatedText().contains("10000"), panel.truncatedText())
            val big = moneyText("18446744073709551615")
            assertEquals("$18446744073709.5516 \u00b7 18446744073709551615\u00b5\$", big)
            assertTrue(!big.contains("E"), "exact big money never goes scientific")
            assertEquals("50.0000%", ppmPercentText(500000L))
            assertEquals("100.0000%", ppmPercentText(1_000_000L))
            assertEquals("0.0001%", ppmPercentText(1L))
        }

        step("I07 read failure is the T29 state with no monetary value and View details") {
            assertEquals("Cost unavailable", OUTCOMES_COST_UNAVAILABLE)
            assertEquals("The current authority has not supplied a cost value.", OUTCOMES_COST_UNAVAILABLE_COPY)
            val model = outcomesPanelModelOf(null, "outcomes read refused (status 503 store_error)")
            assertEquals("unavailable", model.state)
            assertTrue(!model.hasMonetaryValue(), "no monetary value behind a refusal")
            assertTrue(model.metricRows.isEmpty())
            assertTrue(model.runRows.isEmpty())
            var details = 0
            val panel = OutcomesPanel()
            panel.setListener(object : OutcomesPanel.Listener {
                override fun onRefresh() {}
                override fun onOlderPage() {}
                override fun onViewDetails() {
                    details++
                }
            })
            panel.setModel(model)
            assertEquals(OUTCOMES_COST_UNAVAILABLE, panel.unavailableHeadlineText())
            assertEquals(OUTCOMES_COST_UNAVAILABLE_COPY, panel.unavailableCopyText())
            assertEquals("View details", panel.viewDetailsText())
            panel.clickViewDetails()
            assertEquals(1, details, "T29 View details routes to the details surface")
        }

        step("I07 paging is bounded newest-first and reports exhaustion") {
            val page = parseProtocolOutcomesPage(JsonCodec.parse(OUTCOMES_PAGE_JSON).view("test"))
            val model = outcomesPanelModelOf(page)
            assertEquals(42L, model.nextCursor?.completedAtMs)
            assertEquals(true, model.olderEnabled())
            assertEquals(false, model.pagingExhausted)
            val exhausted = outcomesPanelModelOf(page.copy(nextCursor = null))
            assertEquals(false, exhausted.olderEnabled())
            assertEquals(true, exhausted.pagingExhausted)
            val panel = OutcomesPanel()
            var older = 0
            panel.setListener(object : OutcomesPanel.Listener {
                override fun onRefresh() {}
                override fun onOlderPage() {
                    older++
                }
                override fun onViewDetails() {}
            })
            panel.setModel(model)
            assertEquals(true, panel.olderEnabled())
            panel.clickOlder()
            assertEquals(1, older)
            panel.setModel(exhausted)
            assertEquals(false, panel.olderEnabled())
            panel.clickOlder()
            assertEquals(1, older, "an exhausted page never fires a cursor read")
            // The row page stays bounded.
            val runs = StringBuilder()
            for (index in 0 until 250) {
                if (index > 0) runs.append(',')
                runs.append(
                    "{\"runId\":\"run-$index\",\"sessionId\":1,\"taskId\":2," +
                        "\"taskClass\":\"Implementation\",\"riskClass\":\"low\"," +
                        "\"candidateSnapshot\":null,\"finalSnapshot\":\"tm1:x\"," +
                        "\"accepted\":true,\"integrated\":true,\"verified\":true," +
                        "\"verifiedFirstVerificationPass\":true,\"repairAttempts\":0," +
                        "\"modelSequence\":[]," +
                        "\"firstExecutionClass\":\"unknown\",\"finalExecutionClass\":\"unknown\"," +
                        "\"attributionState\":\"complete\",\"moneyState\":\"complete\"," +
                        "\"modelSpendMicro\":\"1\"," +
                        "\"verificationSpendMicro\":\"0\",\"reworkSpendMicro\":\"0\"," +
                        "\"avoidedHostedMicro\":null,\"avoidedHostedMeasurement\":\"unavailable\"," +
                        "\"wallTimeMs\":1," +
                        "\"humanInterventions\":0," +
                        "\"proof\":{\"checksPassed\":0,\"checksRequired\":0,\"coveragePpm\":0}," +
                        "\"changeScale\":{\"files\":0,\"addedLines\":0,\"removedLines\":0}," +
                        "\"rollbackEvents\":0,\"recoveryEvents\":0," +
                        "\"finalBasis\":\"unverified\",\"frontierEscalations\":0," +
                        "\"completedAtMs\":$index}"
                )
            }
            val many = parseProtocolOutcomesPage(
                JsonCodec.parse("{\"metrics\":$OUTCOMES_METRICS_JSON,\"runs\":[$runs]," +
                    "\"nextCursor\":null}").view("test")
            )
            assertEquals(250, many.runs.size)
            assertEquals(OUTCOMES_MAX_RUN_ROWS, outcomesPanelModelOf(many).runRows.size)
        }

        step("I07 typed entitlement refusals render with the value-entitlements status line") {
            // The status line: resolved plan, the outcome_ledger decision and
            // the granted/total count — from the served report only.
            val report = parseNativeValueEntitlements(VALUE_ENTITLEMENTS_JSON)
            assertEquals("org-local", report.organizationId)
            assertEquals(
                "value entitlements: plan pro · outcome_ledger granted · 1/2 granted",
                valueEntitlementsStatusLine(report)
            )
            val refused = report.copy(
                planFound = false,
                planId = null,
                decisions = report.decisions.map {
                    if (it.hook == "outcome_ledger") {
                        it.copy(granted = false, cause = "plan_missing")
                    } else {
                        it
                    }
                }
            )
            assertEquals(
                "value entitlements: plan unresolved · outcome_ledger refused (plan_missing) · " +
                    "0/2 granted",
                valueEntitlementsStatusLine(refused)
            )
            // A withheld decision renders as refused, never granted.
            assertTrue(
                valueEntitlementsStatusLine(report.copy(decisions = emptyList()))
                    .contains("outcome_ledger refused (no decision served)")
            )
            // The refusal line: billing_disabled is the exact local copy.
            assertEquals(
                "value entitlements: billing disabled locally",
                valueEntitlementsRefusalStatusLine("billing_disabled", "409 billing_disabled")
            )
            assertEquals(
                "value entitlements unavailable: cloud unreachable",
                valueEntitlementsRefusalStatusLine("cloud_unavailable", "cloud unreachable")
            )

            // The typed 403 rides the model and renders in the panel next to
            // the value-entitlements line; no monetary value appears.
            val model = outcomesPanelModelOf(
                null,
                "outcomes read refused (status 403 value_entitlement_refused: " +
                    "outcome_analytics not granted)",
                refusalCode = "value_entitlement_refused",
                entitlementsStatus = "value entitlements: plan local · outcome_ledger " +
                    "refused (not_in_plan) · 0/2 granted"
            )
            assertEquals("unavailable", model.state)
            assertEquals("value_entitlement_refused", model.refusalCode)
            assertTrue(!model.hasMonetaryValue(), "no monetary value behind a typed refusal")
            assertTrue(model.lines().contains(model.entitlementsStatus), model.lines().joinToString())
            val panel = OutcomesPanel()
            panel.setModel(model)
            assertTrue(
                panel.statusText().contains("value_entitlement_refused"),
                panel.statusText()
            )
            assertEquals(model.entitlementsStatus, panel.entitlementsStatusText())

            // The typed 409 renders its code and the disabled line.
            val disabled = outcomesPanelModelOf(
                null,
                "409 billing_disabled: commercial billing is disabled",
                refusalCode = "billing_disabled",
                entitlementsStatus = valueEntitlementsRefusalStatusLine("billing_disabled", "")
            )
            val disabledPanel = OutcomesPanel()
            disabledPanel.setModel(disabled)
            assertTrue(
                disabledPanel.statusText().contains("billing_disabled"),
                disabledPanel.statusText()
            )
            assertEquals(
                "value entitlements: billing disabled locally",
                disabledPanel.entitlementsStatusText()
            )

            // No configured organization: the local read carries no status
            // line and the panel hides it.
            val local = outcomesPanelModelOf(
                parseProtocolOutcomesPage(JsonCodec.parse(OUTCOMES_PAGE_JSON).view("test"))
            )
            assertEquals(null, local.entitlementsStatus)
            val localPanel = OutcomesPanel()
            localPanel.setModel(local)
            assertEquals("", localPanel.entitlementsStatusText())
        }
    }

    // --------------------------------------------------- I08 app settings

    private fun faktorSettingsUnitTests() {
        step("I08 apply validates first and persists valid values exactly once") {
            val store = RecordingSettingsStore()
            val controller = FaktorSettingsController(store)
            assertEquals("", controller.read().controlPlaneEndpoint, "endpoint default is EMPTY")
            assertEquals(FaktorSettingsSnapshot.DEFAULT_MUTATION_MODE, controller.read().mutationMode)
            val snapshot = FaktorSettingsSnapshot(
                binaryPath = null,
                provider = "alpha",
                model = "alpha-1",
                mutationMode = "shadow",
                controlPlaneEndpoint = "http://127.0.0.1:9",
                controlPlaneOrganization = "org-local"
            )
            controller.apply(snapshot)
            assertEquals(1, store.writeCalls, "one validated batch write")
            assertEquals(
                "alpha",
                store.writes[0][FaktorSettingKeys.RUN_PROVIDER]
            )
            assertEquals("shadow", store.writes[0][FaktorSettingKeys.RUN_MUTATION_MODE])
            assertEquals("http://127.0.0.1:9", store.writes[0][FaktorSettingKeys.CONTROL_PLANE_ENDPOINT])
            val read = controller.read()
            assertEquals("alpha", read.provider)
            assertEquals("alpha-1", read.model)
            assertEquals("shadow", read.mutationMode)
            assertEquals("http://127.0.0.1:9", read.controlPlaneEndpoint)
            // Run-defaults-only writes keep the binary path and coordinates.
            controller.applyRunDefaults("beta", "beta-1", "default")
            assertEquals(2, store.writeCalls)
            assertEquals(
                setOf(
                    FaktorSettingKeys.RUN_PROVIDER,
                    FaktorSettingKeys.RUN_MODEL,
                    FaktorSettingKeys.RUN_MUTATION_MODE
                ),
                store.writes[1].keys
            )
        }

        step("I08 invalid input raises a typed refusal and writes nothing") {
            val store = RecordingSettingsStore()
            val controller = FaktorSettingsController(store)
            val base = FaktorSettingsSnapshot(null, null, null, "default", "", "")
            try {
                controller.apply(base.copy(mutationMode = "direct"))
                fail("an unknown mutation mode must refuse")
            } catch (e: FaktorSettingsValidationException) {
                assertTrue(e.message!!.contains("mutation mode"), e.message!!)
            }
            try {
                controller.apply(base.copy(controlPlaneEndpoint = "acme-host"))
                fail("a scheme-less endpoint must refuse")
            } catch (e: FaktorSettingsValidationException) {
                assertTrue(e.message!!.contains("endpoint"), e.message!!)
            }
            try {
                controller.apply(base.copy(binaryPath = "bin/faktor"))
                fail("a relative binary path must refuse")
            } catch (e: FaktorSettingsValidationException) {
                assertTrue(e.message!!.contains("absolute"), e.message!!)
            }
            try {
                controller.apply(base.copy(controlPlaneEndpoint = "http://"))
                fail("a host-less endpoint must refuse")
            } catch (e: FaktorSettingsValidationException) {
                Unit
            }
            assertEquals(0, store.writeCalls, "no partial write on any refusal")
            assertTrue(store.values.isEmpty())
        }

        step("I08 reset restores defaults and keeps the PasswordSafe credential") {
            val vault = RecordingVault()
            val credentialStore = ControlPlaneCredentialStore(vault)
            val scope = ControlPlaneScope("http://127.0.0.1:9", "org-local")
            credentialStore.store(scope, "sekret-token-xyz")
            assertEquals(1, vault.setCalls)
            assertEquals("sekret-token-xyz", credentialStore.resolve(scope))
            val store = RecordingSettingsStore()
            val controller = FaktorSettingsController(store)
            val defaults = controller.reset()
            assertEquals(FaktorSettingsSnapshot.DEFAULTS, defaults)
            assertEquals("", defaults.controlPlaneEndpoint)
            assertEquals(FaktorSettingsSnapshot.DEFAULT_MUTATION_MODE, defaults.mutationMode)
            // Reset touched neither the settings store nor the credential row.
            assertEquals(0, store.writeCalls)
            assertEquals(1, vault.setCalls, "reset never writes the credential store")
            assertEquals("sekret-token-xyz", credentialStore.resolve(scope), "the credential survives reset")
        }

        step("I08 the credential never appears in any settings value") {
            val vault = RecordingVault()
            val credentialStore = ControlPlaneCredentialStore(vault)
            val scope = ControlPlaneScope("http://127.0.0.1:9", "org-local")
            credentialStore.store(scope, "sekret-token-xyz")
            val store = RecordingSettingsStore()
            val controller = FaktorSettingsController(store)
            controller.apply(
                FaktorSettingsSnapshot(
                    binaryPath = null,
                    provider = "alpha",
                    model = null,
                    mutationMode = "default",
                    controlPlaneEndpoint = "http://127.0.0.1:9",
                    controlPlaneOrganization = "org-local"
                )
            )
            val serialized = StringBuilder()
            for ((key, value) in store.values) serialized.append(key).append('=').append(value).append('\n')
            for (batchWrite in store.writes) {
                for ((key, value) in batchWrite) serialized.append(key).append('=').append(value).append('\n')
            }
            serialized.append(controller.read().toString())
            assertTrue(
                !serialized.contains("sekret-token-xyz"),
                "the credential value must never enter settings/state: $serialized"
            )
            assertTrue(!serialized.contains("password"), "no credential-shaped key is persisted")
        }

        step("I08 endpoint default stays empty (no vendor substitution)") {
            assertEquals("", FaktorSettingsSnapshot.DEFAULTS.controlPlaneEndpoint)
            val store = RecordingSettingsStore()
            val controller = FaktorSettingsController(store)
            val read = controller.read()
            assertEquals("", read.controlPlaneEndpoint)
            assertTrue(!read.controlPlaneEndpoint.contains("http"))
            assertTrue(!read.controlPlaneEndpoint.lowercase().contains("faktor"))
            val reset = controller.reset()
            assertEquals("", reset.controlPlaneEndpoint)
        }
    }

    private fun step(name: String, body: () -> Unit) {
        try {
            body()
            println("PASS $name")
        } catch (e: Throwable) {
            failures++
            println("FAIL $name: ${e.message}")
        }
    }
}

/** Every rendered text of a component subtree (smoke assertions). */
private fun collectTexts(component: java.awt.Component, out: MutableList<String>) {
    when (component) {
        is WrappedLabel -> out.add(component.fullText)
        is javax.swing.JLabel -> out.add(component.text)
        is javax.swing.text.JTextComponent -> out.add(component.text)
        else -> {}
    }
    if (component is java.awt.Container) {
        for (child in component.components) collectTexts(child, out)
    }
}

private fun textsOf(component: java.awt.Component): List<String> {
    val out = ArrayList<String>()
    collectTexts(component, out)
    return out
}

/** The labels of every direct child of the tree node at [keyPath] (smoke). */
private fun childLabelsOf(panel: TaskTreePanel, keyPath: String): List<String> {
    val root = panel.treeForTest().model.root as? DefaultMutableTreeNode ?: return emptyList()
    fun walk(node: DefaultMutableTreeNode): DefaultMutableTreeNode? {
        if (panel.nodeKeyPathForTest(node) == keyPath) return node
        for (index in 0 until node.childCount) {
            val found = walk(node.getChildAt(index) as DefaultMutableTreeNode)
            if (found != null) return found
        }
        return null
    }
    val node = walk(root) ?: return emptyList()
    val out = ArrayList<String>()
    for (index in 0 until node.childCount) {
        val child = node.getChildAt(index) as DefaultMutableTreeNode
        val payload = child.userObject as? TaskTreeNode ?: continue
        out.add(payload.label)
    }
    return out
}

/** True when the subtree contains a scroll pane (narrow-host guard). */
private fun containsScrollPane(component: java.awt.Component): Boolean {
    if (component is javax.swing.JScrollPane) return true
    if (component is java.awt.Container) {
        for (child in component.components) {
            if (containsScrollPane(child)) return true
        }
    }
    return false
}

/** The distinct row Y positions of a wrapping row (wrap proof). */
private fun rowYs(component: java.awt.Component): Set<Int> {
    val ys = LinkedHashSet<Int>()
    if (component is java.awt.Container) {
        for (child in component.components) {
            if (child.isVisible) ys.add(child.y)
        }
    }
    return ys
}

/** The typed refusal raised by one attachment-plan call (smoke helper). */
private fun refusalOf(body: () -> Unit): AttachmentRefusal {
    try {
        body()
    } catch (e: AttachmentRefusal) {
        return e
    }
    throw AssertionError("expected an AttachmentRefusal, nothing was thrown")
}

/** One bounded JSON response of the attachment-contract fake daemon. */
private fun respondAttachmentJson(
    exchange: com.sun.net.httpserver.HttpExchange,
    status: Int,
    body: String
) {
    val bytes = body.toByteArray(Charsets.UTF_8)
    exchange.responseHeaders.add("Content-Type", "application/json")
    exchange.sendResponseHeaders(status, bytes.size.toLong())
    exchange.responseBody.use { it.write(bytes) }
    exchange.close()
}

/** One bounded raw-byte response of the attachment-contract fake daemon. */
private fun respondAttachmentBytes(
    exchange: com.sun.net.httpserver.HttpExchange,
    status: Int,
    contentType: String,
    body: ByteArray
) {
    exchange.responseHeaders.add("Content-Type", contentType)
    exchange.sendResponseHeaders(status, body.size.toLong())
    try {
        exchange.responseBody.use { it.write(body) }
    } catch (e: java.io.IOException) {
        // The client aborts the read once the byte bound is exceeded.
    } finally {
        exchange.close()
    }
}

// Audits 5/6/16: a PARTIAL generation with a capped fingerprint round and a
// never-hosted index service (the only honest null).
private const val INDEX_COVERAGE_JSON = "{" +
    "\"sessionId\":\"7\"," +
    "\"index_coverage\":{" +
    "\"workspace\":3,\"state\":\"ready\",\"generation\":2,\"published_generation\":2," +
    "\"coverage\":{\"files_seen\":4500,\"files_indexed\":512,\"bytes_indexed\":2048," +
    "\"complete\":false,\"truncated_reason\":\"batch_files\"}," +
    "\"fingerprint\":{\"scanned\":12,\"complete\":false,\"shard\":3,\"round_start\":1," +
    "\"epoch\":4,\"shards_done\":7,\"verify\":true,\"cursors\":[]," +
    "\"truncated_reason\":\"fingerprint_files\"}," +
    "\"freshness\":\"partial\",\"serving\":true}}"
private const val INDEX_COVERAGE_UNHOSTED_JSON = "{\"sessionId\":\"7\",\"index_coverage\":null}"

// ------------------------------------------------- I03/I08 test doubles

/**
 * The I03 host seam double: records the immutable artifacts it receives
 * (bytes + immutable title only). The seam has no path-taking method, so a
 * workspace file can never reach an open through it.
 */
private class RecordingChangeReviewHost : ChangeReviewHost {
    val artifacts = ArrayList<ImmutableArtifact>()
    val diffs = ArrayList<Triple<String, ImmutableArtifact, ImmutableArtifact>>()

    override fun openArtifact(artifact: ImmutableArtifact) {
        artifacts.add(artifact)
    }

    override fun openDiff(title: String, left: ImmutableArtifact, right: ImmutableArtifact) {
        diffs.add(Triple(title, left, right))
    }
}

/** Every button label of a component subtree (control-surface assertions). */
private fun collectButtonTexts(component: java.awt.Component, out: MutableList<String>) {
    if (component is javax.swing.AbstractButton) out.add(component.text ?: "")
    if (component is java.awt.Container) {
        for (child in component.components) collectButtonTexts(child, out)
    }
}

/** In-memory application settings store with write accounting (I08). */
private class RecordingSettingsStore : FaktorSettingsStore {
    val values = LinkedHashMap<String, String>()
    val writes = ArrayList<Map<String, String?>>()
    var writeCalls = 0

    override fun read(key: String): String? = values[key]

    override fun write(values: Map<String, String?>) {
        writeCalls++
        writes.add(LinkedHashMap(values))
        for ((key, value) in values) {
            if (value == null) this.values.remove(key) else this.values[key] = value
        }
    }
}

/** PasswordSafe-shaped fake with write accounting; only a row's value. */
private class RecordingVault : SecretVault {
    private val rows = HashMap<String, String>()
    var setCalls = 0

    override fun getPassword(service: String, account: String): String? =
        rows[service + "|" + account]

    override fun setPassword(service: String, account: String, password: String?) {
        setCalls++
        val key = service + "|" + account
        if (password == null) rows.remove(key) else rows[key] = password
    }
}
