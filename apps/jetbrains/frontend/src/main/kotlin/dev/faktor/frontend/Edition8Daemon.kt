// Revision 8 host contracts: every control activation is a real daemon call,
// a real native setting patch, an explicit navigation, or a typed
// `contract-required` refusal. Nothing here paints a success state that the
// daemon did not acknowledge.
//
// Threading: all HTTP work runs on the Edition8 daemon-thread worker; results
// return to the EDT through ONE invokeLater per completion. No network or
// process call is ever made from an EDT stack frame.
package dev.faktor.frontend

import dev.faktor.backend.NativeClient
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativeApiException
import dev.faktor.shared.view
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import javax.swing.SwingUtilities

/** Bounded EDT/work probes; a harness can assert IO never ran on the EDT. */
internal class Edition8Probes {
    val workSubmitted = AtomicLong()
    val workCompleted = AtomicLong()
    val workFailed = AtomicLong()
    val edtDeliveries = AtomicLong()
    val edtRuns = AtomicLong()
    val maxEdtNanos = AtomicLong()

    fun edtRun(nanos: Long) {
        edtRuns.incrementAndGet()
        var seen = maxEdtNanos.get()
        while (nanos > seen && !maxEdtNanos.compareAndSet(seen, nanos)) seen = maxEdtNanos.get()
    }
}

/** The off-EDT executor behind every screen. */
internal class Edition8Work(val probes: Edition8Probes = Edition8Probes()) : AutoCloseable {

    private val worker = Executors.newSingleThreadExecutor { runnable ->
        Thread(runnable, "faktor-edition8-work").apply { isDaemon = true }
    }

    private val inFlight = AtomicInteger()
    private val closed = AtomicBoolean()

    fun submit(label: String, block: () -> String, done: (Edition8Result) -> Unit) {
        if (closed.get()) {
            done(Edition8Result.Refused("work queue closed: $label", "client"))
            return
        }
        inFlight.incrementAndGet()
        probes.workSubmitted.incrementAndGet()
        val started = System.nanoTime()
        try {
            worker.execute {
                val result = try {
                    Edition8Result.Ok(block())
                } catch (t: Throwable) {
                    if (t is InterruptedException) Thread.currentThread().interrupt()
                    Edition8Result.Failed(describe(t))
                }
                if (result is Edition8Result.Ok) {
                    probes.workCompleted.incrementAndGet()
                } else {
                    probes.workFailed.incrementAndGet()
                }
                if (!closed.get()) {
                    SwingUtilities.invokeLater {
                        val edtStart = System.nanoTime()
                        try {
                            probes.edtDeliveries.incrementAndGet()
                            done(result)
                        } finally {
                            probes.edtRun(System.nanoTime() - edtStart)
                            inFlight.decrementAndGet()
                        }
                    }
                } else {
                    inFlight.decrementAndGet()
                }
            }
        } catch (t: Throwable) {
            inFlight.decrementAndGet()
            done(Edition8Result.Failed("cannot submit $label: ${t.message}"))
        }
    }

    fun awaitIdle(timeoutMs: Long = 10_000): Boolean {
        val deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(timeoutMs)
        while (System.nanoTime() < deadline) {
            if (inFlight.get() == 0) {
                flushEdt()
                return true
            }
            Thread.sleep(5)
        }
        return inFlight.get() == 0
    }

    private fun flushEdt() {
        if (!SwingUtilities.isEventDispatchThread()) {
            try {
                SwingUtilities.invokeAndWait { }
            } catch (ignored: Exception) {
            }
        }
    }

    override fun close() {
        closed.set(true)
        worker.shutdownNow()
    }

    companion object {
        fun describe(t: Throwable): String = when (t) {
            is NativeApiException ->
                "typed ${t.status} ${t.code}: ${bound(t.detail, 300)}" +
                    if (t.retryable) " (retryable)" else ""
            is dev.faktor.shared.NativeProtocolException ->
                "protocol ${t.path}: ${bound(t.detail, 300)}"
            else -> "typed failure: ${bound(t.message ?: t.javaClass.simpleName, 300)}"
        }
    }
}

internal sealed class Edition8Result {
    data class Ok(val body: String) : Edition8Result()
    data class Failed(val reason: String) : Edition8Result()
    data class Refused(val reason: String, val scope: String) : Edition8Result()
}

/**
 * A control is wired only to its own declared route. An unrelated route that
 * happens to answer 200 is NOT a fallback: the old family-based mapping wired
 * e.g. S-04 (provider timeout) to /native/skills and therefore reported the
 * control effective while its capability did not exist. Such controls stay a
 * typed "no acknowledged native route" gap instead.
 */
internal object Edition8Contracts {
    /**
     * Exact capability route for controls whose catalog extraction recorded
     * "no route" even though the daemon registers the capability. The route
     * is chosen per capability (instructions vs skills vs hooks), never a
     * cross-family guess: e.g. S-04 no longer reads /native/skills for a
     * provider-timeout control. A control with no real route stays null.
     */
    fun fallbackRoute(control: Edition8Control): String? = when (control.id) {
        "I-01", "I-03", "I-05", "I-06", "I-07", "I-08", "I-10" -> "/native/instructions/records"
        "I-02", "I-09" -> "/native/instructions"
        "I-04", "I-11" -> "/native/instructions/bindings?session={session}"
        "S-01", "S-02", "S-03", "S-04", "S-05", "S-06", "S-14" -> "/native/skills"
        "S-07", "S-08", "S-09", "S-10", "S-11", "S-12" -> "/native/hooks"
        "S-13" -> "/native/hooks/audit"
        else -> null
    }

    /** Capability seams with no exact daemon route (reported, never faked). */
    fun notes(control: Edition8Control): List<String> = emptyList()
}

/** The one effective configuration snapshot used by every Rev8 setting form. */
internal class Edition8ConfigDraft {
    var revision: Long = -1
        private set
    var runtimeRevision: Long = -1
        private set
    private val values = LinkedHashMap<String, String>()
    private val staged = LinkedHashMap<String, String>()
    private val locked = LinkedHashSet<String>()

    fun committed(savedRevision: Long, runtimeRevision: Long) {
        revision = savedRevision
        this.runtimeRevision = runtimeRevision
    }

    fun load(body: String) {
        val root = JsonCodec.parse(body).view("snapshot")
        revision = root.optionalField("revision")?.long() ?: -1
        runtimeRevision = root.optionalField("runtime_revision")?.long() ?: -1
        values.clear()
        locked.clear()
        val objectValues = root.optionalField("values")?.value as? JsonValue.Obj
        if (objectValues != null) {
            for ((key, value) in objectValues.fields) {
                val effective = value as? JsonValue.Obj
                val v = effective?.fields?.get("value")
                values[key] = when (v) {
                    is JsonValue.Str -> v.value
                    JsonValue.Null -> ""
                    null -> JsonCodec.write(value)
                    else -> JsonCodec.write(v)
                }
                val isLocked = (effective?.fields?.get("locked") as? JsonValue.Bool)?.value == true
                if (isLocked) locked.add(key)
            }
        }
    }

    fun value(key: String): String = staged[key] ?: values[key] ?: ""
    fun isLocked(key: String): Boolean = key in locked
    fun stage(key: String, value: String) {
        if (isLocked(key)) return
        if (values[key] == value) staged.remove(key) else staged[key] = value
    }
    fun changes(): Map<String, String> = LinkedHashMap(staged)
    fun clearStaged() = staged.clear()

    fun patchBody(expectedRevision: Long): String {
        val changes = staged.map { (key, value) ->
            JsonValue.Obj(
                linkedMapOf(
                    "key" to JsonValue.Str(key),
                    "scope" to JsonValue.Str("user"),
                    "value" to JsonValue.Str(value)
                )
            )
        }
        return JsonCodec.write(
            JsonValue.Obj(
                linkedMapOf(
                    "expected_revision" to JsonValue.Int64(expectedRevision),
                    "changes" to JsonValue.Arr(changes)
                )
            )
        )
    }
}

/**
 * The daemon-facing seam. `clientProvider` resolves the ONE live connection;
 * a missing client is a typed prerequisite refusal, never an in-memory fake.
 */
internal class Edition8Daemon(
    private val clientProvider: () -> NativeClient?,
    private val sessionId: () -> String?,
    private val taskId: () -> String?,
    val work: Edition8Work
) {
    val draft = Edition8ConfigDraft()

    fun hasClient(): Boolean = clientProvider() != null

    fun loadConfig(after: (Edition8Result) -> Unit) = call("GET", "/native/config/snapshot", null, after)

    fun readForControl(control: Edition8Control, after: (Edition8Result) -> Unit) {
        val path = endpointFor(control)
        if (path == null) {
            after(
                Edition8Result.Refused(
                    "contract-required: no readable native route named for ${control.id}" +
                        if (control.contractGap.isNotBlank()) " (${control.contractGap})" else "",
                    "catalog"
                )
            )
            return
        }
        call("GET", path, null, after)
    }

    fun applyConfig(after: (Edition8Result) -> Unit) {
        val client = clientProvider()
        if (client == null) {
            after(Edition8Result.Refused("daemon not connected: start Faktor (S34)", "client"))
            return
        }
        if (draft.revision < 0) {
            after(Edition8Result.Refused("settings snapshot not loaded; read before applying", "revision"))
            return
        }
        if (draft.changes().isEmpty()) {
            after(Edition8Result.Refused("nothing staged: edit a value before Apply", "draft"))
            return
        }
        val body = draft.patchBody(draft.revision)
        work.submit("config.preview", {
            client.nativeContractJson("POST", "/native/config/preview", body)
        }) { preview ->
            when (preview) {
                is Edition8Result.Failed, is Edition8Result.Refused -> after(preview)
                is Edition8Result.Ok -> {
                    val valid = JsonCodec.parse(preview.body).view("preview")
                        .optionalField("valid")?.value as? JsonValue.Bool
                    if (valid?.value == false) {
                        after(Edition8Result.Refused("preview refused the patch: ${bound(preview.body, 240)}", "settings"))
                        return@submit
                    }
                    work.submit("config.commit", {
                        client.nativeContractJson("POST", "/native/config/commit", body)
                    }) { commit ->
                        if (commit is Edition8Result.Ok) {
                            try {
                                val root = JsonCodec.parse(commit.body).view("commit")
                                draft.committed(
                                    root.optionalField("saved_revision")?.long() ?: draft.revision,
                                    root.optionalField("runtime_revision")?.long() ?: draft.runtimeRevision
                                )
                                draft.clearStaged()
                            } catch (ignored: Exception) {
                            }
                        }
                        after(commit)
                    }
                }
            }
        }
    }

    /**
     * X-06: validate and launch the real advertised browser adapter. The
     * daemon owns URL validation and launch; the host never invents a
     * connected state.
     */
    fun connectBrowser(url: String, after: (Edition8Result) -> Unit) {
        val session = sessionId()
        if (session == null) {
            after(Edition8Result.Refused("no conversation is bound; browser sessions are session-scoped", "session"))
            return
        }
        val body = JsonCodec.write(
            JsonValue.Obj(
                linkedMapOf(
                    "adapterId" to JsonValue.Str("chromium-local"),
                    "session" to JsonValue.Str(session),
                    "url" to JsonValue.Str(url)
                )
            )
        )
        call("POST", "/native/browser/sessions", body, after)
    }

    fun boundSession(): String? = sessionId()

    fun mcpServers(after: (Edition8Result) -> Unit) = call("GET", "/native/mcp/servers", null, after)

    fun mcpCatalogs(id: String, after: (Edition8Result) -> Unit) =
        call("GET", "/native/mcp/servers/" + java.net.URLEncoder.encode(id, "UTF-8") + "/resources", null, after)

    fun browserSessions(after: (Edition8Result) -> Unit) {
        val session = sessionId()
        if (session == null) {
            after(Edition8Result.Refused("no conversation is bound; browser sessions are session-scoped", "session"))
            return
        }
        call("GET", "/native/browser/sessions?session=" + java.net.URLEncoder.encode(session, "UTF-8"), null, after)
    }

    fun browserObservations(browserId: String, after: (Edition8Result) -> Unit) {
        call("GET", "/native/browser/sessions/" + java.net.URLEncoder.encode(browserId, "UTF-8") + "/observations", null, after)
    }

    fun browserScreenshot(browserId: String, after: (Edition8Result) -> Unit) {
        val body = JsonCodec.write(
            JsonValue.Obj(
                linkedMapOf(
                    "mime" to JsonValue.Str("image/png"),
                    "dataBase64" to JsonValue.Str(""),
                    "inModelContext" to JsonValue.Bool(false)
                )
            )
        )
        call("POST", "/native/browser/sessions/" + java.net.URLEncoder.encode(browserId, "UTF-8") + "/screenshot", body, after)
    }

    fun call(
        method: String,
        path: String,
        body: String?,
        after: (Edition8Result) -> Unit
    ) {
        val client = clientProvider()
        if (client == null) {
            after(Edition8Result.Refused("daemon not connected: start Faktor (S34)", "client"))
            return
        }
        work.submit("$method $path", {
            client.nativeContractJson(method, path, body)
        }) { after(it) }
    }

    /** Resolve `{id}`/`{session}`/`{task}` placeholders from the bound scope. */
    fun resolve(path: String): String? {
        var out = path
        val sid = sessionId()
        val tid = taskId()
        // `{session}` is unambiguous and may appear on capability-scoped
        // queries (e.g. /native/instructions/bindings?session={session}),
        // not only under /native/session/. Resolve it first.
        if (out.contains("{session}")) {
            if (sid == null) return null
            out = out.replace("{session}", sid)
        }
        if (out.contains("{id}") || out.contains("{sid}") ||
            out.contains("{a_sid}") || out.contains("{b_sid}") || out.contains("{session_id}")
        ) {
            if (out.startsWith("/native/tasks/") || out.startsWith("/native/jobs/")) {
                if (tid == null) return null
                out = out.replace("{id}", tid)
            } else if (out.startsWith("/native/session/") || out.startsWith("/native/sessions/")) {
                if (sid == null) return null
                out = out.replace("{id}", sid)
                    .replace("{session}", sid)
                    .replace("{sid}", sid)
                    .replace("{a_sid}", sid)
                    .replace("{b_sid}", sid)
                    .replace("{session_id}", sid)
            } else {
                return null
            }
        }
        if (out.contains("{")) return null
        // Session-scoped collection routes require the bound session query.
        val sidForQuery = sessionId()
        if (sidForQuery != null && out in SESSION_QUERY_ROUTES) {
            out += "?session=" + java.net.URLEncoder.encode(sidForQuery, "UTF-8")
        }
        return out
    }

    private companion object {
        val SESSION_QUERY_ROUTES = setOf(
            "/native/agents",
            "/native/terminals",
            "/native/index/coverage",
            "/native/instructions",
            "/native/memory",
            "/native/permissions"
        )
    }

    fun endpointFor(control: Edition8Control): String? {
        // The generated functional-evidence route map is authoritative: a
        // hand-written catalog hint can be stale, and an unrelated route that
        // happens to answer 200 is never a fallback.
        val mapped = Edition8RouteMap.route(control.id)
        if (mapped != null) {
            when (mapped.kind) {
                "config" -> return resolve("/native/config/snapshot")
                "host" -> return null
                "read" -> return resolve(mapped.path)
                else -> Unit
            }
        }
        for (endpoint in control.endpoints) {
            if (endpoint.state != "exists") continue
            val actual = endpoint.actual
            if (!actual.startsWith("/native/")) continue
            val resolved = resolve(actual) ?: continue
            return resolved
        }
        val fallback = Edition8Contracts.fallbackRoute(control) ?: return null
        return resolve(fallback)
    }

    fun primaryRead(path: String, after: (Edition8Result) -> Unit) = call("GET", path, null, after)

    fun post(path: String, body: String, after: (Edition8Result) -> Unit) =
        call("POST", path, body, after)
}
