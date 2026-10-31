# JetBrains split-mode bridge (Faktor)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../../docs/assets/brand/faktor-lockup-accent.svg">
  <img src="../../docs/assets/brand/faktor-lockup-ink.svg" alt="Faktor" width="300">
</picture>

The JetBrains side of the Faktor split-mode design. The daemon is the
`faktor-cli` binary launched as `serve --port 0`; this tree owns the
process lifecycle, the auth channel, the protocol clients, and a real
frontend panel. There is no placeholder code left in this tree.

This tree is Faktor-owned end to end: the Swing panels below are the ONE
rendering implementation, with no vendored upstream IDE source, corpus or
lockstep pin in the repository (the retired pinned corpus and its manifest
were removed by the Faktor-owned-UI migration).

## Modules

| Module | Contents |
| --- | --- |
| `:shared` | `dev.faktor.shared` — plain Kotlin data classes with zero dependencies. `Protocol.kt` holds only the daemon stdout startup-line and the Faktor-native bearer auth form; `NativeProtocol.kt` is the native surface: a JSON value model, a recursive-descent reader/writer, typed DTO parsers (incl. providers and terminals), and the strict request bodies. |
| `:backend` | `dev.faktor.backend` — `BackendProcessManager` (launch, startup line, bounded stdout drainer, SIGTERM-then-forcible stop), `NativeClient` (bearer-authenticated HTTP client of the native endpoints incl. providers/terminals/output), `NativeEventStream` (SSE journal stream with cursor resume and bounded backoff). |
| `:frontend` | `dev.faktor.frontend` — `FaktorFrontendService` (the UI-free bridge: start/stop/attach/restart, session, task-run/agent/usage/verification/evidence/provider/terminal routing, stream lifecycle and `reconnectStream` cursor resume) and `FaktorChatPanel`, the native Swing tool-window panel. Its inspector navigation is exactly three cluster destinations: **Work** (conversation + current run + ONE composer with the Run options disclosure), **Inspect** (Overview, Plan with Compare approaches, Changes, Verification, and Agents with Roster / Approvals / Coordination) and **History** (prior work only: title, durable time, outcome, search). Settings, Usage and Diagnostics are contextual utilities opened from the header/overflow and dismissed back to the clusters, never destinations; permissions render as the contextual Approvals surface and the board lives under Agents → Coordination. The section panels are `TaskTreePanel`, `BlockersPanel`, `PermissionsPanel`, `TerminalPanel`, `SettingsPanel`, `UsagePanel`, `DiagnosticsPanel`, `HistoryPanel`, `TournamentPanel`, `BoardPanel`, `EvidenceNavigatorPanel`, `AgentsPanel`, `OverviewPanel`, `ChangesPanel`, `VerificationPanel` and `AttachmentsPanel`; `FaktorToolWindowFactory` is the IntelliJ tool-window host. `FaktorFrontendApp` launches the panel standalone (smoke/diagnostic path). `src/main/resources/META-INF/plugin.xml` is the real plugin descriptor (`dev.faktor.jetbrains`, name/vendor `Faktor`, version `0.1.0`, `since-build 241`). |

## Authentication and lifecycle

- `BackendProcessManager` generates a 64-hex password with `SecureRandom`
  and passes it to the child only through `FAKTOR_SERVER_PASSWORD`
  (environment = protected channel; never argv, never disk, never logs).
- The Faktor startup line is the only stdout contract:
  `faktor server listening on http://127.0.0.1:<port>`.
- The native client authenticates every request with
  `Authorization: Bearer <password>` or the equivalent
  `x-faktor-server-password` header. The retired Basic compatibility form
  is gone from the daemon (see the auth migration note in
  `crates/server/src/auth.rs`): there is no downgrade path, and no client
  in this repository sends it.
- `stop()` is SIGTERM first, `destroyForcibly()` only after a 3s grace;
  the stdout drainer and the SSE thread stop with the process.

## Daemon discovery and the app-level install layout

The daemon executable is resolved from MACHINE-scoped inputs only
(`dev.faktor.backend.DaemonBinary`, which has no project/workspace input by
construction):

1. the APPLICATION-level Faktor binary path (`faktor.daemon.binaryPath` in
   the IDE's application properties store, written by the "Locate daemon…"
   action in the missing-daemon dialog),
2. `FAKTOR_BIN`,
3. the app-level signed install layout `~/.faktor/install/launcher` — the
   location the `faktor-cli` installer/updater manages; the launcher
   authenticates the activated release through the signed pointer before
   exec.

A project/repository `target/debug|release` build is never consulted or
executed, and no workspace-scoped setting exists for the daemon. When nothing
is found, the explicit "Start service" action reports the clear, actionable
error ("install into ~/.faktor/install or use \"Locate daemon…\"") and opens
that dialog with a **Locate daemon…** button; the picked executable is stored
at application scope only. The daemon starts ONLY on that explicit action —
creating the tool window never spawns anything.

Where a checksum record exists beside the resolved executable
(`<binary>.sha256`), `BackendProcessManager.start()` refuses a mismatch or a
malformed record before spawn; a missing record is the explicit unverified
state the host warns about and requires the operator to confirm.

## Native protocol routing

| UI action | Endpoint |
| --- | --- |
| health / readiness | `GET /native/health`, `GET /native/ready` |
| new session | `POST /native/session`, `GET /native/sessions` |
| model catalog | `GET /models` |
| send / abort a turn | `POST /native/session/{id}/prompt`, `POST /native/session/{id}/abort` |
| status | `GET /session/{id}/projection`, `GET /native/session/{id}/tasks` |
| transcript | `GET /native/messages`, SSE `GET /native/session/{id}/events?after=<cursor>` |
| journal paging | `GET /native/events` (cursor twin of the SSE stream) |
| task runs | `POST/GET /native/session/{id}/task-runs`, `POST .../{run_id}/cancel` |
| agents | `GET /native/agents`, `POST /native/agents/{id}/{pause,resume,cancel,retry,steer,model,budget}` |
| usage | `GET /native/usage`, `GET /native/session/{id}/usage` |
| verification | `GET /native/session/{id}/verification`, `GET /native/session/{id}/tasks/{task_id}/verification` |
| evidence | `GET /native/evidence/{id}`, `POST /native/evidence/{id}/retrieve` |
| providers | `GET /native/providers` (registry view; `/models` catalog remains the fallback join) |
| terminals | `GET /native/terminals?session={id}`, `GET /native/session/{id}/terminal/events`, `POST /native/session/{id}/terminal` (session-owned spawn), `GET /native/session/{id}/terminals/{terminal_id}/output` (snapshot) |
| history / lifecycle | `GET /native/sessions`, start/stop/attach, `restart()` (session+cursor preserved), `reconnectStream()` (SSE cursor resume) |
| permissions | `GET /native/permissions?session={id}`, `POST /native/permission/reply` (strict `{session_id, permission_id, decision}`; typed 409 `conflict` / `permission_session_mismatch`) |

SSE frames carry `event:`, `id:` (journal sequence = resume cursor) and one
JSON `data:` line; the only frame schema this client understands is
`faktor-native-event/v1`. Heartbeats are ignored and NEVER advance the
durable cursor (even when a nonconforming server attaches an id), so a
durable event can never be silently skipped below one. A durable-stream
protocol violation — malformed frame, discriminator mismatch,
impossible/backward cursor, oversized durable frame, unsupported frame
version or the daemon's terminal `journal_read_failed` frame — is not
transient: the stream lands in the stable `blocked` state at the offending
cursor (no skip action, the cursor does not advance past it) and recovers
only explicitly (`recoverFrom()` — refresh from snapshot / reconnect after a
daemon upgrade / run doctor, or a new stream); transient disconnects
reconnect with bounded exponential backoff from the last delivered frame id,
never duplicating or skipping.

Historical note (pre-native-route clients, retired): older clients used
`POST /session/create`, `GET /session/list` and the
`/api/session/{id}/events?events_after=` stream, treated heartbeats as
cursor-advancing and dropped oversized frames with a skip; those shapes are
no longer the contract.

## Verification

### IntelliJ plugin build and verification (real, Gradle wrapper)

```bash
cd apps/jetbrains
./gradlew verifyPluginProjectConfiguration  # PASS, no configuration issues
./gradlew buildPlugin     # -> frontend/build/distributions/faktor-0.1.0.zip
./gradlew verifyPlugin    # verifier 1.410 vs IC-241.19416.15 -> Compatible
./gradlew build           # all modules + test sources
./gradlew runIde          # real IDE instance with the plugin installed
```

- Kotlin stdlib: `kotlin.stdlib.default.dependency=false` in
  `gradle.properties` plus an explicit
  `compileOnly("org.jetbrains.kotlin:kotlin-stdlib:1.9.22")` in the root
  build (the stdlib bundled by platform 2024.1), per the IntelliJ Platform
  Gradle plugin guidance. The plugin zip bundles no stdlib
  (`faktor-0.1.0.zip` is ~200 KB) and
  `verifyPluginProjectConfiguration` reports no issues.
- The verifier IDE is pinned with
  `pluginVerification { ides { current() } }` so `verifyPlugin` checks the
  already-fetched IntelliJ IDEA Community 2024.1.7 distribution. The
  default `recommended()` set would download every IC release since build
  241 (EAP/RC included).
- Kotlin used to emit synthetic bridges for `ToolWindowFactory` default
  methods, which the verifier reported as 4 deprecated, 2 experimental and
  6 internal API usages. Compiling with `JvmDefaultMode.NO_COMPATIBILITY`
  removes the bridges; the verifier now reports `Compatible` with zero
  problems (report under `frontend/build/reports/pluginVerifier/`).
- `runIde` on this macOS host started the IDE with `Loaded custom
  plugins: Faktor (0.1.0)` in
  `build/intellijPlatform/sandbox/frontend/IC-2024.1.7/log/idea.log` and
  no headless/display error. The harness terminated it after 300 s
  (`timeout` exit 124) and the IDE logged a clean shutdown. No project was
  opened, so the tool window itself was not exercised by that run.
- Only the 2024.1.7 distribution is verified; `until-build` remains
  unbounded, so newer-platform compatibility is not claimed.

The build resolves the Gradle 9.7.1 wrapper distribution from
`services.gradle.org`, the IntelliJ Platform Gradle plugin 2.18.1 and
Kotlin 2.4.20 from the Gradle Plugin Portal / Maven Central, and the
IntelliJ IDEA Community 2024.1.7 installer from the JetBrains CDN
(JDK 17 toolchain). The project cache is redirected under `build/` via
`org.jetbrains.intellij.platform.intellijPlatformCache`, so no generated
state lands outside gitignored directories.

### Real IDE host journey (JetBrains platform integration lane)

The audit-grade interaction journey runs inside the REAL IntelliJ Platform
test application, not an offscreen render harness: it opens a
`ProjectManager` project, registers the `plugin.xml` Faktor tool-window bean
through the platform's own registration API, creates content through the
production `FaktorToolWindowFactory` (the run shows the `Work / Inspect /
History` clusters and the ONE composer), requests composer focus, types
through the composer's own typed-character editor action (the action the
Swing keymap dispatches for `KEY_TYPED`), and dispatches the exact action
bound to Ctrl+Enter while observing the immutable submission id + goal.
Evidence is written to `target/certification/jetbrains-ide-journey/`
(`journey.json` plus `faktor-tool-window.png`, a render of the real
platform-created component tree).

```bash
bash apps/jetbrains/ide-journey.sh                    # probe + run (lane)
cd apps/jetbrains && ./gradlew :frontend:ideJourney   # direct Gradle task
```

- Availability: the lane needs the pinned IntelliJ IDEA Community 2024.1.7
  distribution either cached under `~/.gradle/caches/.../idea/ideaIC/` plus
  the platform test runtime, or reachable from the JetBrains repository.
  `ide-journey.sh` probes both (the network probe is a bounded 6 s HEAD);
  when neither holds it records
  `target/certification/jetbrains-ide-journey/skip.json` and prints
  `SKIPPED` — a skip is never reported as a run.
- What the journey does NOT prove (recorded in `journey.json` as
  `does_not_prove`): OS-level focus ownership (the platform test harness has
  no showing window; the focus request routing is the observable) and daemon
  connectivity (no daemon binary is configured for this lane; the send's
  transport failure is expected and does not fail the journey).
- A plain `:frontend:test` run prints `IDE JOURNEY SKIPPED`; only
  `:frontend:ideJourney` (or `ide-journey.sh`) executes the journey.

### Per-platform visual-baseline acceptance (one command per platform)

Each release platform pins only its OWN render. The acceptance tool is
host-bound: it refuses a record for any platform other than the host it runs
on, requires the `environment` to name that platform (`linux-`, `mac-os-` /
`darwin-`, `windows-`) with the resolved `-f<hex>` font fingerprint, and
requires digests distinct from every other platform's. A linux run can
therefore never write the macos/windows record, and a copied or
fingerprint-less record is refused. One documented command per platform,
run ON that platform:

```bash
# Linux host (renders, validates and pins the linux record):
node scripts/certification/accept-visual-baseline.mjs linux --render
# macOS host:
node scripts/certification/accept-visual-baseline.mjs macos --render
# Windows host, Git Bash (the owned certifying lane also captures the merged
# file at target/certification/visual-baselines-windows.json):
node scripts/certification/accept-visual-baseline.mjs windows --render
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/windows-visual-baseline.ps1 -WriteBaselines
```

`--render` runs `bash apps/jetbrains/compile-and-smoke.sh --write-baselines`
on this host; the Kotlin writer detects the host platform itself (it is never
passed in) and re-pins only that platform's record with its resolved-font
environment fingerprint. To accept a record file produced elsewhere by its
own lane:
`node scripts/certification/accept-visual-baseline.mjs windows target/certification/visual-baselines-windows.json`.
The legacy fingerprint-less `macos` record in this tree stays
`unfingerprinted` (the canonical checker refuses it) until a real Mac host
runs the command above; that is a recorded evidence gap, not a pass.

### Two IDE versions (HAR-16)

The IntelliJ Platform release the frontend resolves is a documented project
property; the default is the committed pin and nothing changes without the
property:

| Property | Default | Meaning |
| --- | --- | --- |
| `faktor.ideaVersion.pin` (`gradle.properties`) | `2024.1.7` | The committed pin — single source of truth for the default release. |
| `faktor.ideaVersion` (CLI `-P`) | *(unset = pin)* | Explicit release selection. The documented alternate is `2024.1`, the oldest stable release in the `241` line the plugin declares support for (`ideaVersion.sinceBuild = "241"`). |
| `faktor.ideaVisual` (CLI `-P`) | *(unset = full)* | Only `structural`: runs an explicitly selected alternate release WITHOUT visual digest certification. Refused on the pin and refused without `faktor.ideaVersion` (fail-closed in `build.gradle.kts` and in `JetBrainsParityMatrix.visualCertStructuralOnly`). |

```bash
# Both legs sequentially (pin first, then the alternate release):
bash apps/jetbrains/two-ide-matrix.sh
# Single legs / plumbing proof (no execution, no IDE download):
bash apps/jetbrains/two-ide-matrix.sh --only-pin
bash apps/jetbrains/two-ide-matrix.sh --only-alt --alt 2024.1
bash apps/jetbrains/two-ide-matrix.sh --dry-run --only-alt   # task wiring proof
```

Per-version artifact strategy (the canonical pinned-release files are never
clobbered; anything a run writes is suffixed with its selected release):

- `target/certification/jetbrains-host-matrix.json` →
  `jetbrains-host-matrix-<release>.json`
- `target/certification/jetbrains-parity.json` → `jetbrains-parity-<release>.json`
- acceptance overlays (`edition5`/`edition8`) are suffixed the same way, and
  each acceptance smoke reads the host-matrix file of ITS OWN run.
- The release id is validated as a filename fragment before any write
  (`ParityPath.requireIdeaVersionId`: `[A-Za-z0-9][A-Za-z0-9._-]*`, no `..`) —
  path traversal is refused, and `ParityPath.artifactNamingPolicySelfTest`
  (adversarial cases included) runs inside both smokes.
- Only explicitly selected releases add `ideaVersion` / `visualCertMode`
  fields to the artifacts: the pin's canonical JSON stays byte-identical.

Visual-baseline honesty (why the alternate leg is structural-only): the
per-platform visual baselines certify `(platform × resolved fonts × PINNED
IDE release)` but the environment fingerprint
(`<os>-<arch>-jvm<v>-f<fonts>`) carries no IDE release. A second release's
digests therefore cannot honestly (a) compare against the pin's records —
different render, identical fingerprint — nor (b) be written into the shared
per-platform files — that would silently relabel them as the pin's certified
claim. The alternate leg keeps every STRUCTURAL assertion (width/zoom/theme/
keyboard/state/registry/catalog/optical thresholds, canned parity rows,
behavioral parity matrix) and skips only digest certification
(`visual.status = structural_only` in its parity artifact); re-pinning
(`--write-baselines` / `writeBaselines`) under a structural leg is refused
fail-closed. Extending certification to a second release would require its
own per-platform baseline file plus real-host review — deliberately not
invented by this change.

Dependency integrity: the committed `gradle.lockfile` +
`gradle/verification-metadata.xml` pin the default release. The pin leg runs
with full locking, byte-identical to before. The alternate leg resolves a
different IDE set, so `frontend/build.gradle.kts` deactivates locking for the
frontend project's configurations only (backend/shared keep theirs; the
integrity gate `scripts/check-gradle-integrity.sh` is unchanged), and the
runner refreshes verification metadata with `--write-verification-metadata
sha256` for the leg — the downloaded alternate distribution is checksummed as
it is verified — then restores the committed file (trap-guarded).

CI: the nightly `ui-smoke-jetbrains` lane runs the existing byte-identical
pin leg first, then this runner's `--only-alt` leg
(`FAKTOR_JETBRAINS_ALT_IDEA_VERSION`, default `2024.1`); a leg failure stops
the lane before its marker, so the lane's `passed` marker always covers both
legs. Cost: one extra IDE distribution download (~700-900 MB) into the
project-local Gradle plugin cache per clean workspace (the `~/.gradle` volume
only caches Maven artifacts) and roughly doubled frontend runtime.

### kotlinc + daemon smoke (fallback, no Gradle)

```bash
bash apps/jetbrains/compile-and-smoke.sh
```

This builds `faktor-cli` if missing and then:

1. compiles `shared + backend + test + frontend` (Swing included) with
   `kotlinc` (kotlin-stdlib.jar from the compiler distribution, no
   network, no Gradle);
2. runs `BackendSmoke <binary>` — the native flow (start →
   health → create session → send message → settle → messages → stop);
   a provider-less daemon answers the message send with HTTP 502 and the
   session lands `failed_recoverable`, both accepted as honest outcomes;
3. runs `NativeBridgeSmoke <binary>` — first the fake-server unit suite
   (`NativeClientTest`: JSON codec, every DTO parser incl. hostile
   payloads, exact method/path/bearer/body routing, typed error mapping,
   body bounds, SSE frames/heartbeat/oversized-frame/reconnect-resume),
   then the real native flow (start → bearer health → ready → create
   session → prompt → SSE frames + cursor → messages → journal page →
   task-runs → agents → usage → verification → task views → typed
   evidence error → stop);
4. runs `FrontendSmoke <binary>` — canned native frames for every panel
   section plus the real daemon (task-run attachments, permissions,
   tournaments, board round-trip, typed refusals, evidence);
5. runs `JetBrainsParitySmoke <binary>` — first the Faktor-owned tree
   check (`FAKTOR-OWNED TREE PASS`: root resolves, no vendored upstream UI
   corpus exists, the panel sources are present, all offline), then the
   thirteen parity families over canned frames (`task mode`, `agent tree with
   blockers/presentation/pixel identity`, `criterion proofs`, `permissions`,
   `terminal`, `review/tournament`, `evidence navigation`, `settings`,
   `provider selection`, `history`, `restart/reconnect`, `change review`,
   `outcomes`), then a raw-socket fake
   daemon driving the REAL `FaktorFrontendService` + `FaktorChatPanel`
   end to end (permission reply, terminal spawn, task-run body,
   session-from-selection, history open, SSE cursor resume), then the
   real daemon (history, task-run with mutation mode, terminal
   spawn/list/events/output, restart with the durable session, reconnect);
6. runs `JetBrainsHostMatrixSmoke` against the BUILT plugin ZIP when one is
   present (`FAKTOR_JETBRAINS_PLUGIN_ZIP` or
   `frontend/build/distributions/faktor-*.zip`): it extracts the ZIP, puts
   its `faktor/lib/*.jar` first on the classpath, asserts the production
   class provenance, and runs every panel through width 240/320/480/800,
   font zoom 100/125/200 %, theme light/dark/high-contrast, keyboard-only
   Tab/SPACE traversal and the empty/loading/error/blocked/reconnect states.
   With `FAKTOR_JETBRAINS_REQUIRE_PLUGIN_ZIP=1` (the trusted
   `jetbrains-smoke` lane) a missing ZIP is a typed failure. The result is
   `target/certification/jetbrains-host-matrix.json`
   (`faktor-jetbrains-host-matrix/v1`, records the ZIP sha256 and class
   location). Equivalent Gradle task: `./gradlew :frontend:smokeHostMatrixZip`
   (or `./gradlew smokeHostMatrixZip`).

The offscreen visual render runs on Linux under the pinned core-fonts
fontconfig
(`frontend/src/test/resources/parity/fonts/core-fonts.conf` set as
`FONTCONFIG_FILE`) with a checkout-local `user.home`; that is the
environment the committed Linux visual record is pinned in. Re-pin with
`./gradlew :frontend:smokeJetBrainsParity -PwriteBaselines=true
-PfaktorCliBin=<bin>` (or the script's `--write-baselines`). A Windows
record is only valid when produced by the Windows-owned
`scripts/windows-visual-baseline.ps1 -WriteBaselines` lane.

Every step prints PASS/FAIL; the script exits nonzero on any failure. The
script compiles only the non-IntelliJ sources, so it stays runnable
without an SDK download.
