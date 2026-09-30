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
import dev.faktor.shared.NativePermissionReplyRefusal
import dev.faktor.shared.NativeMessage
import dev.faktor.shared.NativeProtocolException
import dev.faktor.shared.NativeRequests
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
import dev.faktor.shared.parseNativeSessionUsage
import dev.faktor.shared.parseNativeTaskCompletionSteps
import dev.faktor.shared.parseNativeTaskProof
import dev.faktor.shared.indexCoverageLabel
import dev.faktor.shared.parseNativeIndexCoverage
import dev.faktor.shared.parseNativeTaskVerification
import dev.faktor.shared.parseNativeTaskViews
import dev.faktor.shared.parseNativeTournament
import dev.faktor.shared.parseNativeTournamentDecision
import dev.faktor.shared.parseNativeTournamentStarted
import dev.faktor.shared.parseNativeTournamentSummaries
import dev.faktor.shared.parseNativeVerificationView
import dev.faktor.shared.parseProtocolAttachmentRef
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

        step("Task-mode contract controls are checked only in the Task tab") {
            val panel = FaktorChatPanel(
                FaktorFrontendService(
                    Paths.get("target/debug/faktor-cli"),
                    Paths.get(System.getProperty("java.io.tmpdir"), "faktor-frontend-contract-smoke")
                )
            )
            try {
                assertEquals(null, panel.completionContractFromControls())
                panel.completionCommit.isSelected = true
                panel.completionPr.isSelected = true
                assertEquals(
                    NativeCompletionContract(includeCommit = true, includePush = false, includePr = true),
                    panel.completionContractFromControls()
                )
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
            assertTrue(panel.headerText().contains("unread=2"), panel.headerText())
            assertTrue(panel.headerText().contains("posts=2"), panel.headerText())
            assertTrue(panel.postsText().contains("#3 [child:8] handoff"), panel.postsText())
            assertTrue(panel.postsText().contains("#2 [root] root note"), panel.postsText())
            panel.submitComposer()
            assertEquals("status", postedSubject)
            assertEquals("all green", postedBody)
            // An explicit read acknowledges the page; an automatic refresh
            // never moves the watermark.
            panel.setBoard(parseNativeBoardPage(BOARD_PAGE_JSON), acknowledge = true)
            assertTrue(panel.headerText().contains("unread=0"), panel.headerText())
            panel.reset()
            assertEquals(false, panel.available())
            assertEquals(false, panel.postEnabled())
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
                tournament = parseNativeTournament(TOURNAMENT_JSON)
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
            // The explicit task criterion has no record verdict: unavailable,
            // with the precise reason — never a fabricated pass.
            val explicit = byKey["criterion A"] ?: fail("the explicit criterion must render a proof row")
            assertEquals("unavailable", explicit.verdict)
            assertEquals("unavailable", explicit.bindingKind)
            assertEquals(CriterionVerdictTone.UNAVAILABLE, explicit.tone)
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
            assertTrue(
                byKey["check criterion"]?.verificationTimestamp?.contains("started 11ms") == true,
                byKey["check criterion"]?.verificationTimestamp
            )
            assertTrue(
                byKey["check criterion"]?.verificationTimestamp?.contains("completed 22ms") == true,
                byKey["check criterion"]?.verificationTimestamp
            )
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
                assertTrue(row.unavailable.contains("verdict"), row.unavailable.toString())
                assertTrue(row.unavailable.contains("binding"), row.unavailable.toString())
                assertTrue(row.verdictReason?.contains("no durable verification record") == true, row.verdictReason)
            }
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
            // Drill-down: each durable step + the gate verdict.
            val stepLines = view.stepLines()
            assertTrue(stepLines.any { it == "[succeeded] push — pushed to origin" }, stepLines.toString())
            assertTrue(stepLines.any { it == "gate: satisfied" }, stepLines.toString())
            // The panel renders the proof node from the served DTO.
            val panel = TaskTreePanel()
            panel.update(model)
            assertEquals(model, panel.model())

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
            val attachments = AttachmentsPanel()
            val dir = Files.createTempDirectory("faktor-attach-smoke-")
            val file = Files.createFile(Paths.get(dir.toString(), "note.txt"))
            attachments.addFiles(listOf(file.toString(), file.toString()))
            assertEquals(1, attachments.count())
            assertEquals(1, attachments.files().size)
            assertTrue(!attachments.files()[0].startsWith(".."), "paths must be absolute")
            attachments.clear()
            assertEquals(0, attachments.count())
            // Image parity: the classifier mirrors the daemon allowlist and
            // the bounded read refuses an oversized image before base64.
            assertEquals("image/png", AttachmentImages.mimeOf("shot.PNG"))
            assertEquals("image/jpeg", AttachmentImages.mimeOf(dir.toString() + "/a.jpeg"))
            assertTrue(AttachmentImages.mimeOf(dir.toString() + "/spec.pdf") == null)
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
            val keyA = planAttachments(listOf(file.toString()), emptyList(), policy)
                .uploads.single().key
            // Same path, same length, same mtime; different bytes => fresh identity.
            Files.write(file, bytesB)
            file.toFile().setLastModified(stamp)
            val keyB = planAttachments(listOf(file.toString()), emptyList(), policy)
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
            val keyTwin = planAttachments(listOf(twin.toString()), emptyList(), policy)
                .uploads.single().key
            assertEquals(keyB, keyTwin, "identity is content+filename, not path or mtime")
            // Identical bytes under a DIFFERENT filename is a distinct
            // reference: a rename/re-select must upload fresh.
            val renamed = Paths.get(otherDir.toString(), "renamed.txt")
            Files.write(renamed, bytesB)
            val keyRenamed = planAttachments(listOf(renamed.toString()), emptyList(), policy)
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
        // drives the plan; source files stay repository paths; unsupported
        // MIME / oversize entries are typed refusals before any upload.
        step("attachment plan: documents upload when advertised, source files stay paths") {
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
                policy
            )
            assertEquals(
                listOf(binary.mime, "application/pdf", "text/plain"),
                plan.uploads.map { it.mime },
                "binaries first, then deliverable documents (images must exist on disk)"
            )
            assertEquals(listOf(source.toString(), "Makefile"), plan.pathFiles)
            assertTrue(
                plan.uploads.all { it.base64.isNotEmpty() && it.key.isNotEmpty() },
                "every upload is keyed and base64-ready"
            )
            // A legacy (emergency) policy still uploads documents: the daemon
            // decides admission until the catalog is advertised.
            val emergency = planAttachments(
                listOf(pdf.toString(), txt.toString()), emptyList(),
                AttachmentImages.emergencyPolicy()
            )
            assertEquals(2, emergency.uploads.size)
            assertEquals(0, emergency.pathFiles.size)
            // Unsupported document MIME: the advertised list lacks text/plain.
            val pdfOnly = policy.copy(documentMimes = listOf("application/pdf"))
            val mimeRefusal = refusalOf {
                planAttachments(listOf(txt.toString()), emptyList(), pdfOnly)
            }
            assertEquals("unsupported_document_type", mimeRefusal.code)
            assertTrue(
                mimeRefusal.message!!.contains("deliverable document types"),
                mimeRefusal.message ?: "no message"
            )
            // Advertised model without document input.
            val incapable = refusalOf {
                planAttachments(listOf(pdf.toString()), emptyList(), policy.copy(documentCapable = false))
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
                    policy.copy(maxDocumentBytes = 4L)
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
                    policy.copy(maxDocumentBytes = 4L)
                )
            }
            assertTrue(partial.isFailure, "a refused entry fails the whole plan")
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
                    step("task-run accepts `files` (workspace-relative attachments path)") {
                        val started = client.startTaskRun(
                            sid,
                            "frontend smoke attachment",
                            files = listOf("seed.txt"),
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
