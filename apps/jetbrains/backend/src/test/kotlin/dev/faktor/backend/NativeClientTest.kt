// Adversarial tests for the native-protocol bridge:
//
//   * JSON codec: round trips, hostile input, bounded behavior.
//   * DTO parsers: valid fixtures parse; missing fields / wrong types /
//     trailing junk are loud NativeProtocolExceptions.
//   * NativeClient routing: a fake raw-socket HTTP daemon asserts the exact
//     method+path+Bearer header+body for every endpoint the panel calls,
//     maps 401/500 bodies to typed errors, and enforces the body bound.
//   * NativeEventStream: fake SSE daemon tests frames, heartbeat handling,
//     oversized-frame dropping, reconnect with cursor resume (the second
//     request must carry after=<last delivered id>).
//   * NativeBridgeSmoke: a real `faktor-cli` daemon end-to-end (start,
//     authenticate, create session, prompt, SSE, task-run/agent/usage/
//     verification/evidence routes), mirroring BackendSmoke.
//
// No kotlin.test/JUnit and no network beyond 127.0.0.1: the raw-socket fake
// daemon keeps the whole suite dependency-free and offline.
package dev.faktor.backend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativeApiException
import dev.faktor.shared.NativePermissionReplyRefusal
import dev.faktor.shared.NativeProtocolException
import dev.faktor.shared.MicroMoney
import dev.faktor.shared.NativeRequests
import dev.faktor.shared.parseNativeAgentControlAck
import dev.faktor.shared.parseNativeAgents
import dev.faktor.shared.parseNativeBillingUsage
import dev.faktor.shared.parseNativeBoardPage
import dev.faktor.shared.parseNativeBoardPost
import dev.faktor.shared.parseNativeCreditGrant
import dev.faktor.shared.parseNativeEntitlements
import dev.faktor.shared.parseNativeEvidence
import dev.faktor.shared.parseNativeEvidenceRetrieval
import dev.faktor.shared.parseNativeHealth
import dev.faktor.shared.parseNativeIdentity
import dev.faktor.shared.parseNativeModelCatalog
import dev.faktor.shared.parseNativeProjection
import dev.faktor.shared.parseNativePromptReceipt
import dev.faktor.shared.parseNativeReady
import dev.faktor.shared.parseNativeSessionCreated
import dev.faktor.shared.parseNativeSessionList
import dev.faktor.shared.parseNativeSessionUsage
import dev.faktor.shared.parseNativeTaskRunCancelled
import dev.faktor.shared.parseNativeTaskRunStarted
import dev.faktor.shared.parseNativeTaskRuns
import dev.faktor.shared.parseNativeTaskVerification
import dev.faktor.shared.parseNativeTaskViews
import dev.faktor.shared.parseNativeUsage
import dev.faktor.shared.parseNativeVerificationView
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.ByteArrayOutputStream
import java.math.BigInteger
import java.io.InputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.nio.file.Files
import java.nio.file.Paths
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

object NativeClientTest {

    @JvmStatic
    fun runAll() {
        assertJsonCodec()
        assertRequestBodies()
        assertResponseParsers()
        assertBillingParsers()
        assertMoneyParsers()
        assertHostileParsers()
        assertClientRoutes()
        assertErrorMapping()
        assertBodyBound()
        assertSseStreaming()
        assertSseOversizedFrame()
        println("PASS all native client unit assertions")
    }
}

// ----------------------------------------------------------------- fixtures

private const val HEALTH_JSON = "{\"ok\":true,\"version\":\"1.2.3\"}"

private const val READY_JSON = "{\"ready\":true}"

private const val CREATED_JSON =
    "{\"id\":\"7\",\"title\":\"T\",\"created_ms\":1750000000000}"

private const val SESSIONS_JSON =
    "{\"sessions\":[{\"id\":\"7\",\"title\":\"T\",\"provider\":\"fake\"," +
        "\"model\":\"m\",\"state\":\"ready\"}]}"

private const val MODELS_JSON =
    "[{\"provider\":\"fake\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
        "\"tools\":true,\"parallelTools\":false,\"reasoning\":false,\"thinking\":false," +
        "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
        "\"streaming\":true,\"source\":\"conservativeDefault\"}]"

private const val PROMPT_JSON =
    "{\"op_id\":\"op-1\",\"accepted\":true,\"queued\":false}"

private const val ABORT_JSON = "{\"aborted\":[\"op-1\"]}"

private const val PROJECTION_JSON = "{" +
    "\"session\":{\"id\":\"7\",\"title\":\"T\",\"provider\":\"fake\",\"model\":\"m\"," +
    "\"lifecycle\":\"open\"}," +
    "\"state\":{\"machine\":\"streaming\",\"label\":\"streaming\",\"active\":true," +
    "\"terminal\":false}," +
    "\"activeModel\":{\"provider\":\"fake\",\"model\":\"m\",\"variant\":null}," +
    "\"activeTool\":{\"tool\":\"read_file\",\"opId\":\"op-2\",\"startedMs\":5," +
    "\"status\":\"running\"}," +
    "\"progress\":null," +
    "\"filesChanged\":[\"a.rs\",\"b.rs\"]," +
    "\"lastCheckpoint\":null," +
    "\"verification\":[{\"opId\":\"op-3\",\"tool\":\"shell\",\"startedMs\":6," +
    "\"effectStatus\":\"unknown\"}]," +
    "\"contextUsage\":null,\"queued\":2" +
    "}"

private const val TASKS_JSON = "[" +
    "{\"goal\":\"ship it\",\"constraints\":[\"no network\"],\"state\":\"in_progress\"," +
    "\"milestones\":{\"completed\":[\"m1\"],\"open\":[\"m2\"]}," +
    "\"decisions\":[],\"failures\":[],\"changedFiles\":[\"a.rs\"]," +
    "\"tests\":{\"run\":[\"cargo test\"],\"failed\":[]}," +
    "\"preferences\":[]," +
    "\"verification\":[{\"id\":\"v1\",\"detail\":\"failed:check\",\"status\":\"failed\"}]," +
    "\"progress\":null," +
    "\"budget\":{\"maxTokens\":100,\"maxTurns\":5,\"spentTokens\":7,\"spentTurns\":1," +
    "\"maxCostMicro\":50,\"spentCostMicro\":2,\"openReservedMicro\":0}}" +
    "]"

private const val TASK_RUNS_JSON = "[" +
    "{\"task_id\":3,\"run_id\":\"run-1\",\"mode\":\"in_session\",\"state\":\"Running\"," +
    "\"goal\":\"ship it\",\"item_ids\":[\"main\"],\"model\":\"m\"}" +
    "]"

private const val TASK_RUN_STARTED_JSON =
    "{\"task_id\":3,\"run_id\":\"run-1\",\"state\":\"Running\"}"

private const val TASK_RUN_CANCELLED_JSON =
    "{\"run_id\":\"run-1\",\"cancelled\":true}"

private const val BOARD_PAGE_JSON = "{" +
    "\"board_id\":7,\"revision\":3,\"posts\":[" +
    "{\"id\":3,\"board_id\":7,\"author_child\":8,\"author_session\":8," +
    "\"subject\":\"handoff\",\"body\":\"ready\",\"refs\":[\"evidence:41\"]," +
    "\"revision\":3,\"created_ms\":1700}," +
    "{\"id\":2,\"board_id\":7,\"author_child\":null,\"author_session\":7," +
    "\"subject\":\"root\",\"body\":\"note\",\"refs\":[]," +
    "\"revision\":2,\"created_ms\":1600}" +
    "],\"next_before_revision\":2,\"has_more\":true}"

private const val BOARD_POST_JSON = "{" +
    "\"id\":3,\"board_id\":7,\"author_child\":8,\"author_session\":8," +
    "\"subject\":\"handoff\",\"body\":\"ready\",\"refs\":[]," +
    "\"revision\":3,\"created_ms\":1700}"

private const val AGENTS_JSON = "[" +
    "{\"agent_id\":\"child-1\",\"kind\":\"child\",\"run_id\":\"run-1\"," +
    "\"session_id\":9,\"worktree_id\":1,\"goal\":\"work\",\"state\":\"Running\"," +
    "\"model\":\"m\",\"budget\":1000,\"ownership\":\"Mutating\"," +
    "\"capabilities\":[],\"progress\":null,\"result\":null,\"item_ids\":[\"main\"]}" +
    "]"

private const val AGENT_ACK_JSON = "{\"queuedSeq\":4,\"applied\":false}"

private const val USAGE_JSON = "{" +
    "\"sessions\":2,\"totals\":{\"budget\":100,\"spent\":40},\"perSession\":[]," +
    "\"durable\":{\"sessionsWithCalls\":1," +
    "\"providerCalls\":{\"tokens\":1234,\"prefixObservations\":1," +
    "\"prefixTokens\":100,\"prefixStabilityObservations\":0}," +
    "\"taskSpend\":{\"settledCostMicro\":77}," +
    "\"reservations\":{\"open\":{\"count\":0,\"predictedMicro\":0}," +
    "\"settled\":{\"count\":0,\"predictedMicro\":0,\"spentMicro\":0," +
    "\"providerReportedMicro\":0}," +
    "\"refunded\":{\"count\":0,\"predictedMicro\":0}," +
    "\"uncertain\":{\"count\":0,\"predictedMicro\":0},\"truncated\":false}}," +
    "\"truncated\":false" +
    "}"

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

private const val CREDIT_GRANT_JSON = "{" +
    "\"ok\":true,\"duplicate\":false," +
    "\"credits\":{\"granted_micro\":6000000,\"consumed_micro\":2000000," +
    "\"refunded_micro\":100000,\"held_micro\":250000,\"pending_consumes\":1}" +
    "}"

private const val BILLING_DISABLED_JSON =
    "{\"error\":{\"code\":\"billing_disabled\"," +
        "\"message\":\"commercial billing is disabled (enable the [billing] section to use it)\"," +
        "\"retryable\":false}}"

private const val PERMISSIONS_JSON = "{" +
    "\"permissions\":[" +
    "{\"id\":\"7\",\"session_id\":\"9\",\"capability\":\"shell\"," +
    "\"detail\":{\"tool\":\"bash\"}}," +
    "{\"id\":\"8\",\"session_id\":\"7\",\"capability\":\"write_file\"," +
    "\"detail\":\"write a.rs\"}" +
    "]}"

private const val PERMISSION_ACK_JSON = "{\"ok\":true}"

private const val PERMISSION_CONFLICT_JSON =
    "{\"error\":{\"code\":\"conflict\",\"message\":\"permission 7 unknown or already resolved\"," +
        "\"retryable\":false}}"

private const val PERMISSION_SESSION_MISMATCH_JSON =
    "{\"error\":{\"code\":\"permission_session_mismatch\"," +
        "\"message\":\"permission 7 is owned by session 8, not session 9\"," +
        "\"retryable\":false}}"

private const val SESSION_USAGE_JSON = "{" +
    "\"sessionId\":\"7\",\"providerCalls\":{\"tokens\":10,\"prefixObservations\":[]}," +
    "\"prefixStability\":null,\"tasks\":[{\"taskId\":\"3\"," +
    "\"budget\":{\"maxTokens\":100,\"maxTurns\":5,\"spentTokens\":7,\"spentTurns\":1," +
    "\"maxCostMicro\":50,\"spentCostMicro\":2,\"openReservedMicro\":0}," +
    "\"reservations\":{\"open\":{\"count\":0,\"predictedMicro\":0},\"settled\":{}," +
    "\"refunded\":{},\"uncertain\":{},\"truncated\":false}}]" +
    "}"

private const val VERIFICATION_JSON = "{" +
    "\"owed\":[{\"opId\":\"op-3\",\"tool\":\"shell\",\"startedMs\":6," +
    "\"status\":\"running\",\"effectStatus\":\"unknown\"}]," +
    "\"failedChecks\":[{\"id\":\"v1\",\"detail\":\"failed:check\",\"status\":\"failed\"}]" +
    "}"

private const val TASK_VERIFICATION_JSON = "{" +
    "\"sessionId\":\"7\",\"taskId\":\"3\",\"records\":[{" +
    "\"recordId\":\"r1\",\"revision\":\"rev\",\"workspaceId\":\"1\",\"worktreeId\":\"1\"," +
    "\"treeHash\":null," +
    "\"criteria\":[{\"criterionKey\":\"c1\",\"passed\":true,\"evidence\":null}]," +
    "\"checks\":[{\"check\":\"unit\",\"program\":\"cargo\",\"args\":[\"test\"]," +
    "\"category\":\"test\",\"required\":true,\"status\":\"passed\",\"startedMs\":1," +
    "\"finishedMs\":2,\"exit\":0,\"summary\":null}]," +
    "\"changedFiles\":[{\"path\":\"a.rs\",\"digestHex\":\"aa\",\"size\":3}]," +
    "\"unrelatedChanges\":[],\"reviewer\":null,\"status\":\"passed\"," +
    "\"startedMs\":1,\"completedMs\":2}]}"

private const val EVIDENCE_JSON = "{" +
    "\"id\":9,\"kind\":\"tool_output\",\"sessionId\":7,\"workspaceId\":1," +
    "\"taskId\":null,\"sourceRevision\":null,\"compressibility\":null," +
    "\"backingCompleteness\":null,\"backingRetained\":true,\"backingLen\":5," +
    "\"allowRanges\":true,\"allowSearch\":true,\"maxBytes\":1048576" +
    "}"

/** `{"bytesBase64":"aGVsbG8="}` decodes to `hello`. */
private const val EVIDENCE_RETRIEVAL_JSON = "{" +
    "\"id\":9,\"selector\":{\"selector\":\"all\"},\"bytesBase64\":\"aGVsbG8=\"," +
    "\"byteLen\":5,\"truncatedByPolicy\":false" +
    "}"

private const val MESSAGES_JSON = "{" +
    "\"sessionId\":\"7\",\"messages\":[" +
    "{\"seq\":1,\"id\":1,\"role\":\"user\",\"createdMs\":1,\"data\":{\"text\":\"hi\"}," +
    "\"parts\":[]}," +
    "{\"seq\":2,\"id\":2,\"role\":\"assistant\",\"createdMs\":2,\"data\":{}," +
    "\"parts\":[{\"kind\":\"text\",\"createdMs\":2,\"data\":{\"text\":\"hello\"}}]}" +
    "],\"hasMore\":false,\"nextBefore\":null}"

private const val EVENTS_JSON = "{" +
    "\"sessionId\":\"7\",\"events\":[" +
    "{\"seq\":1,\"kind\":\"session_created\",\"state\":\"idle\",\"opId\":null," +
    "\"tsMs\":1,\"payload\":{}}" +
    "],\"hasMore\":false,\"nextCursor\":null}"

// ------------------------------------------------------------- JSON codec

private fun assertJsonCodec() {
    val value = JsonCodec.parse("{\"a\":1,\"b\":[true,null,\"x\\n\"],\"c\":{\"d\":2.5}}")
    val root = value as JsonValue.Obj
    assertEquals(1L, (root.fields["a"] as JsonValue.Int64).value)
    val array = root.fields["b"] as JsonValue.Arr
    assertEquals(3, array.items.size)
    assertEquals("x\n", (array.items[2] as JsonValue.Str).value)
    val nested = root.fields["c"] as JsonValue.Obj
    assertEquals(2.5, (nested.fields["d"] as JsonValue.Dbl).value)
    assertEquals(
        "{\"a\":1,\"b\":[true,null,\"x\\n\"],\"c\":{\"d\":2.5}}",
        JsonCodec.write(value)
    )
    for (hostile in listOf(
        "{\"a\":1} trailing",
        "\"unterminated",
        "{\"a\":}",
        "[1,2",
        "{\"a\":01x}"
    )) {
        try {
            JsonCodec.parse(hostile)
            fail("hostile JSON must be rejected: $hostile")
        } catch (e: NativeProtocolException) {
            // expected
        }
    }
}

private fun assertRequestBodies() {
    assertEquals(
        "{\"provider\":\"p\",\"model\":\"m\",\"workspace\":\"/ws\",\"title\":\"T\"}",
        NativeRequests.createSession("p", "m", "/ws", "T")
    )
    assertEquals("{\"provider\":\"p\",\"model\":\"m\"}", NativeRequests.createSession("p", "m"))
    assertEquals(
        "{\"session_id\":\"7\",\"prompt\":\"hi\"}",
        NativeRequests.prompt("7", "hi")
    )
    assertEquals("{\"session_id\":\"7\"}", NativeRequests.abort("7"))
    assertEquals(
        "{\"session_id\":\"7\",\"op_id\":\"op\"}",
        NativeRequests.abort("7", "op")
    )
    assertEquals(
        "{\"goal\":\"g\",\"criteria\":[\"c1\"],\"model\":\"m1\",\"max_tokens\":100," +
            "\"max_cost_micro\":200,\"mutation_mode\":\"shadow\"}",
        NativeRequests.startTaskRun(
            "g", listOf("c1"), "m1", 100, BigInteger.valueOf(200), "shadow"
        )
    )
    assertEquals(
        "{\"max_cost_micro\":5}",
        NativeRequests.changeBudget(maxCostMicro = BigInteger.valueOf(5))
    )
    assertEquals("{\"selector\":\"all\"}", NativeRequests.evidenceSelectorAll())
    assertEquals("{\"text\":\"a\\\"b\\n\"}", NativeRequests.steer("a\"b\n"))
    assertEquals(
        "{\"subject\":\"s\",\"body\":\"b\",\"refs\":[\"r1\",\"r2\"]}",
        NativeRequests.boardPost("s", "b", listOf("r1", "r2"))
    )
    assertEquals("{\"subject\":\"s\",\"body\":\"b\"}", NativeRequests.boardPost("s", "b"))
    assertEquals(
        "{\"amount_micro\":1000000,\"reason\":\"top up\"}",
        NativeRequests.grantCredits(BigInteger.valueOf(1_000_000L), "top up")
    )
    assertEquals(
        "{\"amount_micro\":5,\"account_id\":\"acct-1\"}",
        NativeRequests.grantCredits(BigInteger.valueOf(5L), accountId = "acct-1")
    )
    // Permission resolution is CONTEXTUAL: the reply names the owning
    // session alongside the permission id and the decision.
    assertEquals(
        "{\"session_id\":\"9\",\"permission_id\":\"7\",\"decision\":\"allow\"}",
        NativeRequests.permissionReply("9", "7", "allow")
    )
    assertEquals(
        "{\"session_id\":\"9\",\"permission_id\":\"7\",\"decision\":\"deny\"}",
        NativeRequests.permissionReply("9", "7", "deny")
    )
}

// ---------------------------------------------------------- response parsers

private fun assertResponseParsers() {
    assertEquals(true, parseNativeHealth(HEALTH_JSON).ok)
    assertEquals("1.2.3", parseNativeHealth(HEALTH_JSON).version)
    assertEquals(true, parseNativeReady(READY_JSON).ready)
    val created = parseNativeSessionCreated(CREATED_JSON)
    assertEquals("7", created.id)
    assertEquals("T", created.title)
    assertEquals(1750000000000L, created.createdMs)
    assertEquals("fake", parseNativeSessionList(SESSIONS_JSON)[0].provider)
    assertEquals("m", parseNativeModelCatalog(MODELS_JSON)[0].model)
    assertEquals("op-1", parseNativePromptReceipt(PROMPT_JSON).opId)

    val projection = parseNativeProjection(PROJECTION_JSON)
    assertEquals("7", projection.sessionId)
    assertEquals("streaming", projection.machine)
    assertEquals(true, projection.active)
    assertEquals("fake", projection.activeModel!!.provider)
    assertEquals("read_file", projection.activeTool!!.tool)
    assertEquals(listOf("a.rs", "b.rs"), projection.filesChanged)
    assertEquals(1, projection.verification.size)
    assertEquals(2, projection.queued)

    val tasks = parseNativeTaskViews(TASKS_JSON)
    assertEquals("ship it", tasks[0].goal)
    assertEquals(listOf("m1"), tasks[0].milestones.completed)
    assertEquals(7L, tasks[0].budget!!.spentTokens)
    assertEquals(1, tasks[0].testsRun.size)

    val runs = parseNativeTaskRuns(TASK_RUNS_JSON)
    assertEquals("run-1", runs[0].runId)
    assertEquals(3L, runs[0].taskId)
    assertEquals("Running", parseNativeTaskRunStarted(TASK_RUN_STARTED_JSON).state)
    assertEquals(true, parseNativeTaskRunCancelled(TASK_RUN_CANCELLED_JSON).cancelled)

    val board = parseNativeBoardPage(BOARD_PAGE_JSON)
    assertEquals(7L, board.boardId)
    assertEquals(3L, board.revision)
    assertEquals(2, board.posts.size)
    assertEquals(8L, board.posts[0].authorChild)
    assertEquals(null, board.posts[1].authorChild)
    assertEquals(listOf("evidence:41"), board.posts[0].refs)
    assertEquals(2L, board.nextBeforeRevision)
    assertEquals(true, board.hasMore)
    assertEquals(3L, parseNativeBoardPost(BOARD_POST_JSON).revision)

    val agents = parseNativeAgents(AGENTS_JSON)
    assertEquals("child-1", agents[0].agentId)
    assertEquals(1000L, agents[0].budget)
    val ack = parseNativeAgentControlAck(AGENT_ACK_JSON)
    assertEquals(4L, ack.queuedSeq)
    assertEquals(false, ack.applied)

    val usage = parseNativeUsage(USAGE_JSON)
    assertEquals(2L, usage.sessions)
    assertEquals(1234L, usage.durableTokens)
    assertEquals(BigInteger.valueOf(77L), usage.settledCostMicro)
    assertEquals("7", parseNativeSessionUsage(SESSION_USAGE_JSON).sessionId)
    assertEquals(10L, parseNativeSessionUsage(SESSION_USAGE_JSON).tokens)

    val verification = parseNativeVerificationView(VERIFICATION_JSON)
    assertEquals(1, verification.owed.size)
    assertEquals("failed:check", verification.failedChecks[0].detail)
    val taskVerification = parseNativeTaskVerification(TASK_VERIFICATION_JSON)
    assertEquals(1, taskVerification.records[0].criteriaPassed)
    assertEquals(listOf("unit=passed"), taskVerification.records[0].checks)

    val evidence = parseNativeEvidence(EVIDENCE_JSON)
    assertEquals(9L, evidence.id)
    assertEquals(true, evidence.allowSearch)
    val retrieval = parseNativeEvidenceRetrieval(EVIDENCE_RETRIEVAL_JSON)
    assertEquals("hello", String(retrieval.bytes, Charsets.UTF_8))
    assertEquals(5L, retrieval.byteLen)

    val messages = dev.faktor.shared.parseNativeMessagePage(MESSAGES_JSON)
    assertEquals("hi", messages.messages[0].text)
    assertEquals("hello", messages.messages[1].text)
    val events = dev.faktor.shared.parseNativeEventPage(EVENTS_JSON)
    assertEquals("session_created", events.events[0].kind)
}

private fun assertBillingParsers() {
    val usage = parseNativeBillingUsage(BILLING_USAGE_JSON)
    assertEquals("org-local", usage.organization)
    assertEquals(BigInteger.valueOf(700_000L), usage.fold.totals.managedCostMicro)
    assertEquals(BigInteger.valueOf(200_000L), usage.fold.totals.byokCostMicro)
    assertEquals(1_775L, usage.fold.totals.totalTokens())
    assertEquals(3L, usage.fold.perTask[0].taskId)
    assertEquals("r1", usage.fold.perTask[0].runId)
    assertEquals(BigInteger.valueOf(3_100_000L), usage.credits.balanceMicro())
    assertEquals(BigInteger.valueOf(250_000L), usage.credits.heldMicro)
    assertEquals(1, usage.itemCount)
    assertEquals("9", usage.nextCursor)

    val entitlements = parseNativeEntitlements(ENTITLEMENTS_JSON)
    assertEquals("pro", entitlements.planId)
    assertEquals(true, entitlements.planFound)
    assertEquals(true, entitlements.subscriptionActive)
    assertEquals("active", entitlements.subscriptionStatus)
    assertEquals(BigInteger.valueOf(100_000L), entitlements.limits["max_tokens_per_period"])
    assertEquals(4, entitlements.limits.size)
    assertEquals(1_775L, entitlements.totalTokens)
    assertEquals(1, entitlements.inFlight.size)
    assertEquals("integration", entitlements.inFlight[0].kind)
    assertEquals(null, entitlements.inFlight[0].endedMs)

    val identity = parseNativeIdentity(IDENTITY_JSON)
    assertEquals("admin", identity.role)
    assertEquals("org-local", identity.organization)
    assertEquals(true, identity.effectiveActions.contains("credits_grant"))

    val grant = parseNativeCreditGrant(CREDIT_GRANT_JSON)
    assertEquals(false, grant.duplicate)
    assertEquals(BigInteger.valueOf(6_000_000L), grant.credits.grantedMicro)

    // Hostile billing payloads are loud protocol violations (never a
    // silently wrong panel).
    for (text in listOf(
        "{}",
        "{\"organization\":\"o\",\"fold\":{},\"credits\":{},\"items\":[],\"nextCursor\":null}",
        "{\"organization\":\"o\",\"fold\":{\"organization_id\":\"o\",\"totals\":{}," +
            "\"per_task\":[],\"next_cursor\":null}," +
            "\"credits\":{\"granted_micro\":1,\"consumed_micro\":0,\"refunded_micro\":0," +
            "\"held_micro\":0,\"pending_consumes\":0},\"items\":[],\"nextCursor\":0}",
        "{\"ok\":true,\"entitlements\":{\"organization_id\":\"o\",\"plan_found\":true," +
            "\"subscription_active\":false,\"features\":[]," +
            "\"limits\":{\"max_tokens_per_period\":\"many\"},\"credits\":{}," +
            "\"managed_spend_micro\":0,\"byok_spend_micro\":0,\"total_tokens\":0," +
            "\"in_flight\":[],\"now_ms\":0}}"
    )) {
        try {
            parseNativeBillingUsage(text)
            fail("hostile billing usage must be rejected: $text")
        } catch (e: NativeProtocolException) {
            // expected
        }
    }
    try {
        parseNativeEntitlements(
            "{\"ok\":true,\"entitlements\":{\"organization_id\":\"o\",\"plan_found\":true," +
                "\"subscription_active\":false,\"features\":[]," +
                "\"limits\":{\"max_tokens_per_period\":\"many\"},\"credits\":{}," +
                "\"managed_spend_micro\":0,\"byok_spend_micro\":0,\"total_tokens\":0," +
                "\"in_flight\":[],\"now_ms\":0}}"
        )
        fail("a hostile limit value must be rejected")
    } catch (e: NativeProtocolException) {
        // expected
    }
    try {
        parseNativeIdentity("{\"ok\":true,\"identity\":{\"subject_kind\":\"user\"," +
            "\"subject_id\":\"u\",\"display_name\":\"d\",\"organization\":\"o\"," +
            "\"organization_name\":\"O\",\"role\":\"admin\",\"effective_actions\":{}}}")
        fail("hostile effective_actions must be rejected")
    } catch (e: NativeProtocolException) {
        // expected
    }
}

/**
 * Exact money (the `*_micro` decimal-string protocol change): strings parse
 * exactly at the whole range, legacy numbers are tolerated only while exactly
 * representable (<= 2^53-1) and a larger number is refused loudly rather
 * than silently rounded; display and aggregation are exact BigInteger.
 */
private fun assertMoneyParsers() {
    // 1. Decimal strings parse exactly at 0, 2^53-1, 2^53 and i64::MAX.
    assertEquals(BigInteger.ZERO, MicroMoney.parseDecimal("0"))
    assertEquals(
        BigInteger.valueOf(9007199254740991L),
        MicroMoney.parseDecimal("9007199254740991")
    )
    assertEquals(BigInteger("9007199254740992"), MicroMoney.parseDecimal("9007199254740992"))
    assertEquals(MicroMoney.I64_MAX, MicroMoney.parseDecimal("9223372036854775807"))
    // The served u64 domain is exact end to end.
    assertEquals(
        BigInteger("9223372036854775808"),
        MicroMoney.parseDecimal("9223372036854775808")
    )
    assertEquals(MicroMoney.U64_MAX, MicroMoney.parseDecimal("18446744073709551615"))
    assertEquals(null, MicroMoney.parseDecimal("18446744073709551616"))
    assertEquals(null, MicroMoney.parseDecimal("-1"))
    assertEquals(null, MicroMoney.parseDecimal("1e3"))
    assertEquals(null, MicroMoney.parseDecimal("1.5"))
    assertEquals(null, MicroMoney.parseDecimal(""))
    assertEquals(null, MicroMoney.parseDecimal(" 1"))

    // End to end: a string-money billing payload is exact.
    val usage = parseNativeBillingUsage(creditsPayload("\"9223372036854775807\""))
    assertEquals(MicroMoney.I64_MAX, usage.credits.heldMicro)
    assertEquals(
        BigInteger("9007199254740992"),
        parseNativeBillingUsage(creditsPayload("\"9007199254740992\"")).credits.heldMicro
    )

    // 2. Legacy numbers <= 2^53-1 convert exactly.
    assertEquals(BigInteger.ZERO, MicroMoney.fromNumber(0L))
    assertEquals(
        BigInteger.valueOf(9007199254740991L),
        MicroMoney.fromNumber(9007199254740991L)
    )
    assertEquals(
        BigInteger.valueOf(5_000_000L),
        parseNativeBillingUsage(BILLING_USAGE_JSON).credits.grantedMicro
    )
    // Above 2^53-1 the number is flagged, never rounded.
    assertEquals(null, MicroMoney.fromNumber(9007199254740992L))
    assertEquals(null, MicroMoney.fromNumber(-1L))
    for (text in listOf(
        creditsPayload("9007199254740992"),
        creditsPayload("\"12.5\""),
        creditsPayload("\"-1\"")
    )) {
        try {
            parseNativeBillingUsage(text)
            fail("an unsafe money value must be rejected: $text")
        } catch (e: NativeProtocolException) {
            // expected: loud refusal, never a silent round
        }
    }
    // The limit map is exact too: strings are accepted, unsafe numbers refused.
    assertEquals(
        MicroMoney.I64_MAX,
        parseNativeEntitlements(
            entitlementsPayload("\"9223372036854775807\"")
        ).limits["max_managed_spend_micro_per_period"]
    )
    try {
        parseNativeEntitlements(entitlementsPayload("9007199254740992"))
        fail("an unsafe limit number must be rejected")
    } catch (e: NativeProtocolException) {
        // expected
    }

    // 3. Display is exact: no precision loss, no scientific notation.
    assertEquals("9223372036854775807\u00b5\$", MicroMoney.microText(MicroMoney.I64_MAX))
    assertEquals("9223372036854.7758", MicroMoney.usdText(MicroMoney.I64_MAX))
    assertEquals("1.2346", MicroMoney.usdText(BigInteger.valueOf(1_234_567L)))
    assertEquals("0.5000", MicroMoney.usdText(BigInteger.valueOf(500_000L)))
    assertEquals("0.0000", MicroMoney.usdText(BigInteger.ZERO))
    assertTrue(
        !MicroMoney.microText(MicroMoney.I64_MAX).contains("e") &&
            !MicroMoney.microText(MicroMoney.I64_MAX).contains("E"),
        "money display must never use scientific notation"
    )

    // 4. Aggregation is exact (never a float, saturating balance).
    assertEquals(
        BigInteger("9007199254740992"),
        BigInteger.valueOf(9007199254740991L) + BigInteger.ONE
    )
    assertEquals(
        MicroMoney.I64_MAX,
        MicroMoney.balance(MicroMoney.I64_MAX, BigInteger.ONE, BigInteger.ONE)
    )
    assertEquals(
        BigInteger.ZERO,
        MicroMoney.balance(BigInteger.ZERO, BigInteger.ZERO, BigInteger.valueOf(5L))
    )

    // 5. Request projection: a number while lossless, else the exact string.
    assertEquals(
        "{\"max_cost_micro\":5}",
        NativeRequests.changeBudget(maxCostMicro = BigInteger.valueOf(5L))
    )
    // Above 2^53-1 the request carries the EXACT decimal string.
    assertEquals(
        "{\"max_cost_micro\":\"9223372036854775807\"}",
        NativeRequests.changeBudget(maxCostMicro = MicroMoney.I64_MAX)
    )
    assertEquals(
        "{\"amount_micro\":\"9007199254740992\"}",
        NativeRequests.grantCredits(BigInteger("9007199254740992"))
    )
}

private fun creditsPayload(heldJson: String): String =
    "{\"ok\":true,\"organization\":\"o\"," +
        "\"fold\":{\"organization_id\":\"o\",\"totals\":{\"input_tokens\":0," +
        "\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0," +
        "\"reasoning_tokens\":0,\"provider_cost_micro\":0,\"managed_cost_micro\":0," +
        "\"byok_cost_micro\":0,\"events\":0,\"corrected_events\":0}," +
        "\"per_task\":[],\"next_cursor\":null}," +
        "\"credits\":{\"granted_micro\":0,\"consumed_micro\":0,\"refunded_micro\":0," +
        "\"held_micro\":$heldJson,\"pending_consumes\":0}," +
        "\"items\":[],\"nextCursor\":null}"

private fun entitlementsPayload(limitJson: String): String =
    "{\"ok\":true,\"entitlements\":{\"organization_id\":\"o\",\"plan_found\":true," +
        "\"subscription_active\":false,\"features\":[]," +
        "\"limits\":{\"max_managed_spend_micro_per_period\":$limitJson}," +
        "\"credits\":{\"granted_micro\":0,\"consumed_micro\":0,\"refunded_micro\":0," +
        "\"held_micro\":0,\"pending_consumes\":0}," +
        "\"managed_spend_micro\":0,\"byok_spend_micro\":0,\"total_tokens\":0," +
        "\"in_flight\":[],\"now_ms\":0}}"

private fun assertHostileParsers() {
    val hostile = listOf(
        "{}",
        "{\"ok\":\"true\",\"version\":\"1\"}",
        "{\"ok\":true}",
        "{\"ok\":true,\"version\":\"1\"} junk"
    )
    for (text in hostile) {
        try {
            parseNativeHealth(text)
            fail("hostile health must be rejected: $text")
        } catch (e: NativeProtocolException) {
            // expected
        }
    }
    try {
        parseNativeEvidenceRetrieval(
            "{\"id\":1,\"selector\":{},\"bytesBase64\":\"!!!\",\"byteLen\":1," +
                "\"truncatedByPolicy\":false}"
        )
        fail("invalid base64 must be rejected")
    } catch (e: NativeProtocolException) {
        // expected
    }
    // Board drift: a missing page field, a typed author and a phantom post
    // step all fail loudly (never a silently empty board).
    for (text in listOf(
        "{\"board_id\":7,\"revision\":3,\"posts\":[],\"next_before_revision\":null}",
        "{\"board_id\":7,\"revision\":3,\"posts\":[" +
            "{\"id\":3,\"board_id\":7,\"author_child\":\"root\",\"author_session\":7," +
            "\"subject\":\"s\",\"body\":\"b\",\"refs\":[],\"revision\":3," +
            "\"created_ms\":1}],\"next_before_revision\":null,\"has_more\":false}",
        "{\"board_id\":7,\"revision\":\"3\",\"posts\":[]," +
            "\"next_before_revision\":null,\"has_more\":false}",
        "{\"board_id\":7,\"revision\":3,\"posts\":{}," +
            "\"next_before_revision\":null,\"has_more\":false}"
    )) {
        try {
            parseNativeBoardPage(text)
            fail("hostile board page must be rejected: $text")
        } catch (e: NativeProtocolException) {
            // expected
        }
    }
}

// --------------------------------------------------------------- fake daemon

/** One captured request from the fake daemon. */
private class FakeRequest(
    val method: String,
    val path: String,
    val query: Map<String, String>,
    val headers: Map<String, String>,
    val body: String
)

private class FakeSseWriter(private val out: BufferedOutputStream) {
    fun write(text: String) {
        out.write(text.toByteArray(Charsets.UTF_8))
        out.flush()
    }

    fun comment(text: String) {
        write(": $text\n")
    }

    fun frame(id: Long?, event: String, data: String) {
        if (id != null) write("id: $id\n")
        write("event: $event\n")
        write("data: $data\n\n")
    }
}

private class FakeResponse(private val out: BufferedOutputStream) {
    fun json(status: Int, body: String) {
        send(status, "application/json", body.toByteArray(Charsets.UTF_8))
    }

    fun text(status: Int, body: String) {
        send(status, "text/plain", body.toByteArray(Charsets.UTF_8))
    }

    private fun send(status: Int, contentType: String, bytes: ByteArray) {
        val head = "HTTP/1.1 $status X\r\nContent-Type: $contentType\r\n" +
            "Content-Length: ${bytes.size}\r\nConnection: close\r\n\r\n"
        out.write(head.toByteArray(Charsets.UTF_8))
        out.write(bytes)
        out.flush()
    }

    fun stream(status: Int, contentType: String, block: (FakeSseWriter) -> Unit) {
        val head = "HTTP/1.1 $status X\r\nContent-Type: $contentType\r\n" +
            "Cache-Control: no-cache\r\nConnection: close\r\n\r\n"
        out.write(head.toByteArray(Charsets.UTF_8))
        out.flush()
        block(FakeSseWriter(out))
    }
}

/**
 * Raw-socket HTTP/1.1 fake: exact method+path routing, request capture,
 * chunk-free streaming responses. One thread per connection; all threads
 * are daemons, so a test can abandon a held SSE stream.
 */
private class FakeDaemon {
    private val server = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
    val requests: MutableList<FakeRequest> = Collections.synchronizedList(ArrayList<FakeRequest>())
    private val handlers =
        Collections.synchronizedMap(HashMap<String, (FakeRequest, FakeResponse) -> Unit>())
    @Volatile private var running = true
    private var acceptThread: Thread? = null

    val baseUrl: String
        get() = "http://127.0.0.1:" + server.localPort

    fun on(method: String, path: String, handler: (FakeRequest, FakeResponse) -> Unit) {
        handlers["$method $path"] = handler
    }

    fun start() {
        val thread = Thread({ acceptLoop() }, "fake-daemon-accept")
        thread.isDaemon = true
        acceptThread = thread
        thread.start()
    }

    fun stop() {
        running = false
        try {
            server.close()
        } catch (e: Exception) {
            // Already closed.
        }
        try {
            acceptThread?.join(500)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
        }
    }

    fun requestCount(method: String, path: String): Int = synchronized(requests) {
        requests.count { it.method == method && it.path == path }
    }

    private fun acceptLoop() {
        while (running) {
            val socket = try {
                server.accept()
            } catch (e: Exception) {
                break
            }
            val thread = Thread({ handle(socket) }, "fake-daemon-conn")
            thread.isDaemon = true
            thread.start()
        }
    }

    private fun handle(socket: java.net.Socket) {
        try {
            socket.use {
                val input = BufferedInputStream(socket.getInputStream())
                val requestLine = readLine(input) ?: return
                val parts = requestLine.split(' ')
                if (parts.size < 2) return
                val method = parts[0]
                val target = parts[1]
                val qIndex = target.indexOf('?')
                val path = if (qIndex < 0) target else target.substring(0, qIndex)
                val query = LinkedHashMap<String, String>()
                if (qIndex >= 0) {
                    for (pair in target.substring(qIndex + 1).split('&')) {
                        if (pair.isEmpty()) continue
                        val eq = pair.indexOf('=')
                        if (eq < 0) {
                            query[pair] = ""
                        } else {
                            query[pair.substring(0, eq)] = pair.substring(eq + 1)
                        }
                    }
                }
                val headers = LinkedHashMap<String, String>()
                while (true) {
                    val header = readLine(input) ?: return
                    if (header.isEmpty()) break
                    val colon = header.indexOf(':')
                    if (colon > 0) {
                        headers[lowerAscii(header.substring(0, colon).trim())] =
                            header.substring(colon + 1).trim()
                    }
                }
                val length = headers["content-length"]?.toIntOrNull() ?: 0
                val bodyBytes = ByteArray(length)
                var read = 0
                while (read < length) {
                    val n = input.read(bodyBytes, read, length - read)
                    if (n < 0) break
                    read += n
                }
                val request = FakeRequest(
                    method, path, query, headers,
                    String(bodyBytes, 0, read, Charsets.UTF_8)
                )
                requests.add(request)
                val response = FakeResponse(BufferedOutputStream(socket.getOutputStream()))
                val handler = handlers["$method $path"]
                if (handler == null) {
                    response.json(
                        404,
                        "{\"error\":{\"code\":\"not_found\",\"message\":\"no route\"," +
                            "\"retryable\":false}}"
                    )
                } else {
                    handler(request, response)
                }
            }
        } catch (e: Exception) {
            // Client disconnect; the test already has what it needs.
        }
    }

    /** ASCII-only lowercase (works on kotlinc 1.3, unlike String.lowercase()). */
    private fun lowerAscii(text: String): String {
        val sb = StringBuilder(text.length)
        for (c in text) {
            sb.append(if (c in 'A'..'Z') (c + 32).toChar() else c)
        }
        return sb.toString()
    }

    private fun readLine(input: InputStream): String? {
        val out = StringBuilder()
        while (true) {
            val c = input.read()
            if (c < 0) return if (out.isEmpty()) null else out.toString()
            if (c == '\n'.toInt()) return out.toString()
            if (c != '\r'.toInt()) out.append(c.toChar())
            if (out.length > 65536) return out.toString()
        }
    }
}

// ------------------------------------------------------------- client routes

private fun assertClientRoutes() {
    val daemon = FakeDaemon()
    daemon.on("GET", "/native/health") { _, response -> response.json(200, HEALTH_JSON) }
    daemon.on("GET", "/native/ready") { _, response -> response.json(200, READY_JSON) }
    daemon.on("POST", "/native/session") { _, response -> response.json(200, CREATED_JSON) }
    daemon.on("GET", "/native/sessions") { _, response -> response.json(200, SESSIONS_JSON) }
    daemon.on("GET", "/models") { _, response -> response.json(200, MODELS_JSON) }
    daemon.on("POST", "/native/session/7/prompt") { _, response -> response.json(200, PROMPT_JSON) }
    daemon.on("POST", "/native/session/7/abort") { _, response -> response.json(200, ABORT_JSON) }
    daemon.on("GET", "/session/7/projection") { _, response ->
        response.json(200, PROJECTION_JSON)
    }
    daemon.on("GET", "/native/messages") { _, response -> response.json(200, MESSAGES_JSON) }
    daemon.on("GET", "/native/events") { _, response -> response.json(200, EVENTS_JSON) }
    daemon.on("GET", "/native/session/7/tasks") { _, response -> response.json(200, TASKS_JSON) }
    daemon.on("GET", "/native/session/7/board") { _, response -> response.json(200, BOARD_PAGE_JSON) }
    daemon.on("POST", "/native/session/7/board") { _, response -> response.json(201, BOARD_POST_JSON) }
    daemon.on("GET", "/native/session/7/task-runs") { _, response ->
        response.json(200, TASK_RUNS_JSON)
    }
    daemon.on("POST", "/native/session/7/task-runs") { _, response ->
        response.json(200, TASK_RUN_STARTED_JSON)
    }
    daemon.on("POST", "/native/session/7/task-runs/run-1/cancel") { _, response ->
        response.json(200, TASK_RUN_CANCELLED_JSON)
    }
    daemon.on("GET", "/native/agents") { _, response -> response.json(200, AGENTS_JSON) }
    for (action in listOf("pause", "resume", "cancel", "retry", "steer", "model", "budget")) {
        daemon.on("POST", "/native/agents/child-1/$action") { _, response ->
            response.json(200, AGENT_ACK_JSON)
        }
    }
    daemon.on("GET", "/native/usage") { request, response ->
        if (request.query["org"] != null) {
            response.json(200, BILLING_USAGE_JSON)
        } else {
            response.json(200, USAGE_JSON)
        }
    }
    daemon.on("GET", "/native/identity") { _, response -> response.json(200, IDENTITY_JSON) }
    daemon.on("GET", "/native/entitlements") { _, response ->
        response.json(200, ENTITLEMENTS_JSON)
    }
    daemon.on("POST", "/native/credits/grant") { _, response ->
        response.json(200, CREDIT_GRANT_JSON)
    }
    daemon.on("POST", "/native/sso/logout") { _, response ->
        response.json(200, "{\"ok\":true,\"revoked\":true,\"alreadyRevoked\":false}")
    }
    daemon.on("GET", "/native/session/7/usage") { _, response ->
        response.json(200, SESSION_USAGE_JSON)
    }
    daemon.on("GET", "/native/session/7/verification") { _, response ->
        response.json(200, VERIFICATION_JSON)
    }
    daemon.on("GET", "/native/session/7/tasks/3/verification") { _, response ->
        response.json(200, TASK_VERIFICATION_JSON)
    }
    daemon.on("GET", "/native/evidence/9") { _, response -> response.json(200, EVIDENCE_JSON) }
    daemon.on("POST", "/native/evidence/9/retrieve") { _, response ->
        response.json(200, EVIDENCE_RETRIEVAL_JSON)
    }
    daemon.on("GET", "/native/permissions") { _, response ->
        response.json(200, PERMISSIONS_JSON)
    }
    daemon.on("POST", "/native/permission/reply") { _, response ->
        response.json(200, PERMISSION_ACK_JSON)
    }
    daemon.start()
    try {
        val client = NativeClient(daemon.baseUrl, "tok")
        assertEquals(true, client.health().ok)
        client.createSession("p", "m", "/ws", "T")
        client.prompt("7", "hi")
        client.abortSession("7", "op-1")
        assertEquals("streaming", client.projection("7").machine)
        assertEquals(2, client.messages("7", limit = 20).messages.size)
        assertEquals(1, client.events("7", after = 0).events.size)
        assertEquals("run-1", client.taskRuns("7")[0].runId)
        assertEquals("run-1", client.taskRunState("7", "run-1").runId)
        client.startTaskRun("7", "g")
        client.cancelTaskRun("7", "run-1")
        assertEquals("handoff", client.board("7", since = 9L, limit = 2L).posts[0].subject)
        assertEquals(3L, client.boardPost("7", "handoff", "ready", listOf("evidence:41")).revision)
        assertEquals("child-1", client.agents("7")[0].agentId)
        client.pauseAgent("child-1")
        client.resumeAgent("child-1")
        client.cancelAgent("child-1")
        client.retryAgent("child-1")
        client.steerAgent("child-1", "note")
        client.setAgentModel("child-1", "m")
        client.setAgentBudget("child-1", maxTokens = 1000)
        assertEquals(2L, client.usage().sessions)
        assertEquals("7", client.sessionUsage("7").sessionId)
        assertEquals("admin", client.identity().role)
        assertEquals("pro", client.entitlements().planId)
        assertEquals(
            BigInteger.valueOf(700_000L),
            client.billingUsage("org-local", since = "9", limit = 25L).fold.totals.managedCostMicro
        )
        assertEquals("9", client.billingUsage("org-local").nextCursor)
        assertEquals(
            false,
            client.grantCredits(
                BigInteger.valueOf(1_000_000L), "selftest-key-1", "top up"
            ).duplicate
        )
        assertEquals(1, client.verification("7").owed.size)
        assertEquals("3", client.taskVerification("7", "3").taskId)
        assertEquals(9L, client.evidence("7", 9).id)
        assertEquals("hello", String(client.retrieveEvidence("7", 9, "{\"selector\":\"all\"}").bytes))

        // Permissions: the pending list carries the OWNING session per entry;
        // the reply body is the strict session-scoped DTO.
        val pending = client.permissions("9")
        assertEquals(2, pending.size)
        assertEquals("7", pending[0].id)
        assertEquals("9", pending[0].sessionId)
        assertEquals("shell", pending[0].capability)
        assertEquals("{\"tool\":\"bash\"}", pending[0].detail)
        assertEquals("write a.rs", pending[1].detail)
        assertEquals(true, client.replyPermission("9", "7", "allow").ok)

        val first = daemon.requests[0]
        assertEquals("Bearer tok", first.headers["authorization"])
        val create = daemon.requests.first { it.method == "POST" && it.path == "/native/session" }
        assertEquals(
            "{\"provider\":\"p\",\"model\":\"m\",\"workspace\":\"/ws\",\"title\":\"T\"}",
            create.body
        )
        val prompt = daemon.requests.first { it.path == "/native/session/7/prompt" }
        assertEquals("{\"session_id\":\"7\",\"prompt\":\"hi\"}", prompt.body)
        val boardRead = daemon.requests.first {
            it.method == "GET" && it.path == "/native/session/7/board"
        }
        assertEquals("9", boardRead.query["since"])
        assertEquals("2", boardRead.query["limit"])
        val boardPost = daemon.requests.first {
            it.method == "POST" && it.path == "/native/session/7/board"
        }
        assertEquals("{\"subject\":\"handoff\",\"body\":\"ready\",\"refs\":[\"evidence:41\"]}", boardPost.body)
        for (action in listOf("pause", "resume", "cancel", "retry", "steer", "model", "budget")) {
            assertEquals(
                1,
                daemon.requestCount("POST", "/native/agents/child-1/$action")
            )
        }
        val billingRead = daemon.requests.first {
            it.method == "GET" && it.path == "/native/usage" && it.query["org"] != null
        }
        assertEquals("org-local", billingRead.query["org"])
        assertEquals("9", billingRead.query["since"])
        assertEquals("25", billingRead.query["limit"])
        val grant = daemon.requests.first {
            it.method == "POST" && it.path == "/native/credits/grant"
        }
        assertEquals("selftest-key-1", grant.headers["idempotency-key"])
        assertEquals(
            "{\"amount_micro\":1000000,\"reason\":\"top up\"}",
            grant.body
        )
        val permissionsRead = daemon.requests.first {
            it.method == "GET" && it.path == "/native/permissions"
        }
        assertEquals("9", permissionsRead.query["session"])
        val permissionReply = daemon.requests.first {
            it.method == "POST" && it.path == "/native/permission/reply"
        }
        assertEquals(
            "{\"session_id\":\"9\",\"permission_id\":\"7\",\"decision\":\"allow\"}",
            permissionReply.body
        )
        assertEquals(1, daemon.requestCount("POST", "/native/permission/reply"))
        // The control-plane credential rides the identity/billing reads only
        // when the operator configured one: absent by default, exact when set.
        assertEquals(null, daemon.requests.first {
            it.method == "GET" && it.path == "/native/identity"
        }.headers["x-faktor-control-token"])
        val privileged = NativeClient(daemon.baseUrl, "tok", controlToken = "cp-selftest")
        assertEquals("admin", privileged.identity().role)
        val claimed = daemon.requests.last {
            it.method == "GET" && it.path == "/native/identity"
        }
        assertEquals("cp-selftest", claimed.headers["x-faktor-control-token"])
        // Sign-out names the auth session in the STRICT route body and
        // presents the session's own token alongside the daemon password.
        privileged.revokeControlSession("org-local", "ses-1")
        val logout = daemon.requests.last {
            it.method == "POST" && it.path == "/native/sso/logout"
        }
        assertEquals("Bearer tok", logout.headers["authorization"])
        assertEquals("cp-selftest", logout.headers["x-faktor-control-token"])
        assertEquals(
            "{\"organization\":\"org-local\",\"session_id\":\"ses-1\"}",
            logout.body
        )
        // An incomplete body is refused before it can leave the client.
        try {
            privileged.revokeControlSession(" ", "ses-1")
            fail("a blank organization must be refused")
        } catch (e: NativeProtocolException) {
            if (!e.message.orEmpty().contains("auth-session id")) throw e
        }
        try {
            privileged.revokeControlSession("org-local", "")
            fail("a blank session id must be refused")
        } catch (e: NativeProtocolException) {
            if (!e.message.orEmpty().contains("auth-session id")) throw e
        }
    } finally {
        daemon.stop()
    }
}

private fun assertErrorMapping() {
    val daemon = FakeDaemon()
    daemon.on("GET", "/native/health") { _, response ->
        response.json(
            401,
            "{\"error\":{\"code\":\"unauthorized\",\"message\":\"bad password\"," +
                "\"retryable\":false}}"
        )
    }
    daemon.on("GET", "/native/ready") { _, response ->
        response.text(500, "boom")
    }
    daemon.on("GET", "/native/entitlements") { _, response ->
        response.json(409, BILLING_DISABLED_JSON)
    }
    var permissionReplies = 0
    daemon.on("POST", "/native/permission/reply") { _, response ->
        permissionReplies += 1
        if (permissionReplies == 1) {
            response.json(409, PERMISSION_CONFLICT_JSON)
        } else {
            response.json(409, PERMISSION_SESSION_MISMATCH_JSON)
        }
    }
    daemon.start()
    try {
        val client = NativeClient(daemon.baseUrl, "wrong")
        try {
            client.health()
            fail("401 must throw")
        } catch (e: NativeApiException) {
            assertEquals(401, e.status)
            assertEquals("unauthorized", e.code)
            assertEquals(false, e.retryable)
        }
        try {
            client.ready()
            fail("500 must throw")
        } catch (e: NativeApiException) {
            assertEquals(500, e.status)
            assertEquals("http_error", e.code)
        }
        try {
            client.entitlements()
            fail("409 billing_disabled must throw")
        } catch (e: NativeApiException) {
            assertEquals(409, e.status)
            assertEquals("billing_disabled", e.code)
            assertEquals(false, e.retryable)
        }
        // Typed permission-reply refusals: unknown/expired/already resolved
        // is `conflict`, a live waiter of another session is
        // `permission_session_mismatch`; both stay typed (no blind retry).
        try {
            client.replyPermission("9", "7", "allow")
            fail("409 conflict must throw")
        } catch (e: NativeApiException) {
            assertEquals(409, e.status)
            assertEquals("conflict", e.code)
            assertEquals(false, e.retryable)
            assertEquals(true, NativePermissionReplyRefusal.isUnknownOrResolved(e))
            val text = NativePermissionReplyRefusal.describe(e, "7")
            assertTrue(
                text != null && text.contains("already resolved"),
                "the refusal text must name the resolution authority: $text"
            )
        }
        try {
            client.replyPermission("9", "7", "allow")
            fail("409 permission_session_mismatch must throw")
        } catch (e: NativeApiException) {
            assertEquals(409, e.status)
            assertEquals("permission_session_mismatch", e.code)
            assertEquals(false, e.retryable)
            assertEquals(true, NativePermissionReplyRefusal.isSessionMismatch(e))
            val text = NativePermissionReplyRefusal.describe(e, "7")
            assertTrue(
                text != null && text.contains("different session"),
                "the refusal text must name the ownership conflict: $text"
            )
        }
        assertEquals(
            2,
            daemon.requestCount("POST", "/native/permission/reply"),
            "a typed permission refusal is never retried by the client"
        )
        // An unrelated 409 is never classified as a permission refusal.
        assertEquals(
            null,
            NativePermissionReplyRefusal.describe(
                NativeApiException(409, "shadow_unregistered", "no shadow", false), "7"
            )
        )
    } finally {
        daemon.stop()
    }
}

private fun assertBodyBound() {
    val daemon = FakeDaemon()
    val big = ByteArray(4096) { 'x'.toInt().toByte() }
    daemon.on("GET", "/native/health") { _, response ->
        response.json(200, "{\"ok\":true,\"version\":\"" + String(big) + "\"}")
    }
    daemon.start()
    try {
        val client = NativeClient(daemon.baseUrl, "tok", maxBodyBytes = 64)
        try {
            client.health()
            fail("oversized body must be rejected")
        } catch (e: NativeProtocolException) {
            assertTrue(
                e.detail.contains("exceeded bound"),
                "detail must name the bound: ${e.detail}"
            )
        }
    } finally {
        daemon.stop()
    }
}

// ------------------------------------------------------------------- SSE

private fun awaitLatch(latch: CountDownLatch, timeoutMs: Long, what: String) {
    if (!latch.await(timeoutMs, TimeUnit.MILLISECONDS)) {
        fail("timed out waiting for $what")
    }
}

private fun assertSseStreaming() {
    val daemon = FakeDaemon()
    val held = CountDownLatch(1)
    daemon.on("GET", "/native/session/42/events") { request, response ->
        val after = request.query["after"]?.toLongOrNull() ?: 0L
        if (after < 1L) {
            response.stream(200, "text/event-stream") { writer ->
                writer.comment("keep-alive")
                writer.write("event: heartbeat\ndata: {}\n\n")
                writer.frame(1, "agent_state_changed", "{\"event\":\"agent_state_changed\",\"state\":\"streaming\"}")
            }
        } else {
            response.stream(200, "text/event-stream") { writer ->
                writer.frame(2, "agent_state_changed", "{\"event\":\"agent_state_changed\",\"state\":\"ready\"}")
                held.await(10, TimeUnit.SECONDS)
            }
        }
    }
    daemon.start()
    val events = Collections.synchronizedList(ArrayList<NativeSseEvent>())
    val delivered = CountDownLatch(2)
    val errors = Collections.synchronizedList(ArrayList<String>())
    val stream = NativeEventStream(
        daemon.baseUrl, "tok", "42", 0,
        minBackoffMs = 10, maxBackoffMs = 50,
        onEvent = { event ->
            events.add(event)
            delivered.countDown()
        },
        onError = { error -> errors.add(error.message ?: "error") }
    )
    try {
        stream.start()
        awaitLatch(delivered, 10_000, "two SSE frames")
        assertEquals(2, events.size)
        assertEquals(1L, events[0].id)
        assertEquals(2L, events[1].id)
        assertEquals(2L, stream.cursor)
        assertTrue(errors.isEmpty(), "no frame errors expected: $errors")
        val second = daemon.requests.first {
            it.path == "/native/session/42/events" && it.query["after"] == "1"
        }
        assertEquals("1", second.query["after"])
        assertEquals("Bearer tok", second.headers["authorization"])
        assertEquals("text/event-stream", second.headers["accept"])
    } finally {
        stream.stop()
        held.countDown()
        daemon.stop()
    }
}

private fun assertSseOversizedFrame() {
    val daemon = FakeDaemon()
    val held = CountDownLatch(1)
    val bigData = StringBuilder("{\"x\":\"")
    for (i in 0 until 200) bigData.append('a')
    bigData.append("\"}")
    daemon.on("GET", "/native/session/9/events") { _, response ->
        response.stream(200, "text/event-stream") { writer ->
            writer.frame(9, "agent_state_changed", bigData.toString())
            writer.frame(10, "agent_state_changed", "{\"event\":\"agent_state_changed\"}")
            held.await(10, TimeUnit.SECONDS)
        }
    }
    daemon.start()
    val events = Collections.synchronizedList(ArrayList<NativeSseEvent>())
    val delivered = CountDownLatch(1)
    val errors = Collections.synchronizedList(ArrayList<String>())
    val stream = NativeEventStream(
        daemon.baseUrl, "tok", "9", 0,
        maxFrameBytes = 64, minBackoffMs = 10, maxBackoffMs = 50,
        onEvent = { event ->
            events.add(event)
            delivered.countDown()
        },
        onError = { error -> errors.add(error.message ?: "error") }
    )
    try {
        stream.start()
        awaitLatch(delivered, 10_000, "the valid frame after the oversized one")
        assertEquals(1, events.size)
        assertEquals(10L, events[0].id)
        assertEquals(10L, stream.cursor)
        assertTrue(errors.isNotEmpty(), "oversized frame must be reported loudly")
        assertTrue(
            errors.any { it.contains("exceeded") },
            "error must name the bound: $errors"
        )
    } finally {
        stream.stop()
        held.countDown()
        daemon.stop()
    }
}

// ------------------------------------------------------- real-daemon smoke

/**
 * End-to-end native bridge smoke against a REAL daemon binary (args[0]):
 * lifecycle -> bearer auth -> session -> prompt -> SSE -> projections ->
 * task-run/agent/usage/verification/evidence routes -> stop.
 */
object NativeBridgeSmoke {

    private var failures = 0

    @JvmStatic
    fun main(args: Array<String>) {
        if (args.isEmpty()) {
            println("FAIL usage: NativeBridgeSmoke <faktor-cli binary path>")
            kotlin.system.exitProcess(1)
        }
        val binary = Paths.get(args[0])
        val dataDir = Files.createTempDirectory("faktor-native-smoke-")

        step("native protocol unit assertions") { NativeClientTest.runAll() }

        val manager = BackendProcessManager(binary, dataDir)
        var connection: BackendConnection? = null
        var stream: NativeEventStream? = null
        try {
            step("start daemon (startup line + port)") {
                connection = manager.start()
                println("  port=${connection!!.port} pid=${connection!!.pid()}")
            }
            val conn = connection
            if (conn != null) {
                val client = NativeClient.forConnection(conn, timeoutMs = 180_000L)
                step("native health via bearer auth") {
                    val health = client.health()
                    if (!health.ok) fail("native health ok=false")
                    println("  version=${health.version}")
                }
                step("native readiness") {
                    val ready = client.awaitReady(10_000L)
                    if (!ready.ready) fail("daemon never became ready")
                }
                var sessionId: String? = null
                step("create native session (POST /native/session)") {
                    val created = client.createSession(
                        "default", "default", null, "native bridge smoke"
                    )
                    sessionId = created.id
                    if (created.id.isEmpty()) fail("empty session id")
                    println("  session=${created.id}")
                }
                val sid = sessionId
                if (sid != null) {
                    val events = Collections.synchronizedList(ArrayList<NativeSseEvent>())
                    step("open SSE journal stream (cursor 0)") {
                        val s = NativeEventStream.forConnection(
                            conn, sid, 0,
                            onEvent = { event -> events.add(event) },
                            onError = { error -> println("  sse error: ${error.message}") }
                        )
                        stream = s
                        s.start()
                    }
                    step("prompt (POST /native/session/{id}/prompt)") {
                        val receipt = client.prompt(sid, "ping from native bridge smoke")
                        if (!receipt.accepted) fail("prompt not accepted")
                        println("  op=${receipt.opId} queued=${receipt.queued}")
                    }
                    step("projection settles (ready | failed_*)") {
                        val deadline = System.currentTimeMillis() + 20_000L
                        var machine = "unknown"
                        while (System.currentTimeMillis() < deadline) {
                            machine = client.projection(sid).machine
                            if (machine == "ready_for_next_turn" ||
                                machine == "failed_recoverable" ||
                                machine == "failed_permanent" ||
                                machine == "ready"
                            ) {
                                break
                            }
                            Thread.sleep(200L)
                        }
                        println("  machine=$machine")
                        if (machine != "ready_for_next_turn" &&
                            machine != "failed_recoverable" &&
                            machine != "failed_permanent" &&
                            machine != "ready"
                        ) {
                            fail("session did not settle: $machine")
                        }
                    }
                    step("messages page carries the prompt") {
                        val page = client.messages(sid, limit = 20)
                        if (page.messages.isEmpty()) fail("no messages")
                        if (page.messages.none { it.text.contains("ping from native bridge smoke") }) {
                            fail("prompt not visible in messages page")
                        }
                    }
                    step("SSE delivered frames and advanced the cursor") {
                        if (events.isEmpty()) fail("no SSE frames")
                        println("  frames=${events.size} cursor=${stream!!.cursor}")
                        if (stream!!.cursor <= 0L) fail("cursor did not advance")
                    }
                    step("journal event page (cursor twin)") {
                        val page = client.events(sid, after = 0, limit = 64)
                        if (page.events.isEmpty()) fail("no journal events")
                    }
                    step("task-run listing (GET task-runs)") {
                        println("  runs=${client.taskRuns(sid).size}")
                    }
                    step("agent listing (GET native agents)") {
                        println("  agents=${client.agents(sid).size}")
                    }
                    step("global usage (GET /native/usage)") {
                        val usage = client.usage()
                        println("  sessions=${usage.sessions} durableTokens=${usage.durableTokens}")
                    }
                    step("session usage (GET session usage)") {
                        val usage = client.sessionUsage(sid)
                        if (usage.sessionId != sid) fail("usage session mismatch")
                    }
                    step("verification view") {
                        val verification = client.verification(sid)
                        println(
                            "  owed=${verification.owed.size} failed=${verification.failedChecks.size}"
                        )
                    }
                    step("task views (GET session tasks)") {
                        println("  tasks=${client.taskViews(sid).size}")
                    }
                    step("evidence access is typed (404 unknown / 503 unwired)") {
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
            stream?.stop()
            if (connection != null) {
                val c = connection!!
                try {
                    manager.stop(c)
                    if (c.process.isAlive) fail("daemon still alive after stop")
                    println("PASS stop daemon")
                } catch (e: Throwable) {
                    failures++
                    println("FAIL stop daemon: ${e.message}")
                    c.process.destroyForcibly()
                }
            }
            dataDir.toFile().deleteRecursively()
        }
        println(if (failures == 0) "NATIVE SMOKE PASS" else "NATIVE SMOKE FAIL ($failures)")
        kotlin.system.exitProcess(if (failures == 0) 0 else 1)
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
