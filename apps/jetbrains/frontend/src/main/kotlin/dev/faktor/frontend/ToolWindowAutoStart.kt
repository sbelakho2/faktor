// The real tool-window lifecycle auto-start (trusted-install parity with the
// VS Code supervisor: opening Faktor resolves the machine-scoped,
// trust-verified executable and starts it automatically; only genuine
// authority interrupts the user).
//
// Policy ([ToolWindowAutoStart.decide]):
//  - ONLY a checksum-VERIFIED executable inside the trusted install root
//    (~/.faktor/install, the faktor-cli installer/updater target) may start
//    automatically. Nothing else executes unattended: a missing binary, a
//    checksum mismatch, an unverified binary (no record) or a binary
//    outside the trusted root is a TYPED refusal the host surfaces; the
//    explicit "Start service" action remains the only path for those (it
//    resolves, reports and — for unverified bytes — confirms with the
//    operator).
//  - a live serve.lock holder of the same data dir is ADOPTED through the
//    daemon's reuse contract (FaktorFrontendService.start tries adoption
//    before any spawn), so a reload or a second window joins the running
//    daemon instead of spawning a competitor.
//  - every failure surfaces typed; nothing ever prompts from this path.
//
// IntelliJ-free on purpose: the factory (the only platform file) delegates
// here, so the shipped lifecycle policy is smoke-testable without the IDE.
package dev.faktor.frontend

import dev.faktor.backend.DaemonBinary
import java.nio.file.Path

/** The typed auto-start decision of one tool-window lifecycle start. */
internal sealed class AutoStartDecision {
    /** The verified trusted-install executable may start (or adopt). */
    data class Start(val detail: String) : AutoStartDecision()

    /** A typed authority state: nothing may start unattended. */
    data class Refused(val kind: String, val detail: String) : AutoStartDecision()
}

/**
 * The typed lifecycle outcome the host surfaces: a start/adopt carries the
 * exact version; a refusal carries its machine kind + detail (the panel
 * renders the human copy).
 */
internal data class AutoStartOutcome(
    val started: Boolean,
    val adopted: Boolean,
    val version: String?,
    val refusalKind: String?,
    val refusalDetail: String?
)

internal object ToolWindowAutoStart {

    /** Typed refusal kinds (stable for assertions and Diagnostics detail). */
    const val KIND_BINARY_MISSING = "binary_missing"
    const val KIND_OUTSIDE_TRUSTED_INSTALL = "outside_trusted_install"
    const val KIND_TRUST_UNVERIFIED = "trust_unverified"
    const val KIND_TRUST_REFUSED = "trust_refused"
    const val KIND_STARTUP_FAILED = "startup_failed"

    /**
     * The unattended-start policy over one machine resolution: verified +
     * inside the trusted root starts; everything else is a typed refusal.
     * [resolution] is a lazy result so a missing binary never reaches a
     * trust probe; [trustOf] is injectable for the policy matrix.
     */
    fun decide(
        resolution: () -> DaemonBinary.Resolution,
        trustedRoot: Path,
        trustOf: (Path) -> DaemonBinary.Trust = { DaemonBinary.checksumTrust(it) }
    ): AutoStartDecision {
        val resolved = try {
            resolution()
        } catch (e: Exception) {
            return AutoStartDecision.Refused(
                KIND_BINARY_MISSING,
                e.message ?: e.javaClass.simpleName
            )
        }
        when (val trust = trustOf(resolved.executable)) {
            is DaemonBinary.Trust.Refused ->
                return AutoStartDecision.Refused(KIND_TRUST_REFUSED, trust.detail)
            is DaemonBinary.Trust.Unverified ->
                return AutoStartDecision.Refused(KIND_TRUST_UNVERIFIED, trust.detail)
            is DaemonBinary.Trust.Verified -> {}
        }
        val root = trustedRoot.toAbsolutePath().normalize()
        val executable = resolved.executable.toAbsolutePath().normalize()
        if (!executable.startsWith(root)) {
            return AutoStartDecision.Refused(
                KIND_OUTSIDE_TRUSTED_INSTALL,
                "the resolved daemon $executable is outside the trusted install root $root " +
                    "(source: ${resolved.source})"
            )
        }
        return AutoStartDecision.Start(
            "verified executable inside the trusted install root (${resolved.source})"
        )
    }

    /**
     * One lifecycle auto-start: [decide] first, then the service start (which
     * itself adopts a live serve.lock holder before spawning). Runs OFF the
     * EDT (spawn + bounded readiness wait); the caller surfaces the typed
     * outcome.
     */
    fun lifecycleAutoStart(
        service: FaktorFrontendService,
        resolve: () -> DaemonBinary.Resolution,
        trustedRoot: Path,
        trustOf: (Path) -> DaemonBinary.Trust = { DaemonBinary.checksumTrust(it) }
    ): AutoStartOutcome {
        when (val decision = decide(resolve, trustedRoot, trustOf)) {
            is AutoStartDecision.Refused ->
                return AutoStartOutcome(
                    started = false,
                    adopted = false,
                    version = null,
                    refusalKind = decision.kind,
                    refusalDetail = decision.detail
                )
            is AutoStartDecision.Start -> {}
        }
        return try {
            val health = service.start()
            AutoStartOutcome(
                started = true,
                adopted = service.lastStartAdopted,
                version = health.version,
                refusalKind = null,
                refusalDetail = null
            )
        } catch (e: Exception) {
            AutoStartOutcome(
                started = false,
                adopted = false,
                version = null,
                refusalKind = KIND_STARTUP_FAILED,
                refusalDetail = e.message ?: e.javaClass.simpleName
            )
        }
    }
}
