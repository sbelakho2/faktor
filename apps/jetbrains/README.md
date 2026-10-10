# JetBrains split-mode bridge (Faktor)

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

## Native protocol routing

| UI action | Endpoint |
| --- | --- |
| health / readiness | `GET /native/health`, `GET /native/ready` |
| new session | `POST /session/create`, `GET /session/list` |
| model catalog | `GET /models` |
| send / abort a turn | `POST /session/prompt`, `POST /native/session/{id}/abort` |
| status | `GET /session/{id}/projection`, `GET /native/session/{id}/tasks` |
| transcript | `GET /native/messages`, SSE `GET /api/session/{id}/events?events_after=` |
| journal paging | `GET /native/events` (cursor twin of the SSE stream) |
| task runs | `POST/GET /native/session/{id}/task-runs`, `POST .../{run_id}/cancel` |
| agents | `GET /native/agents`, `POST /native/agents/{id}/{pause,resume,cancel,retry,steer,model,budget}` |
| usage | `GET /native/usage`, `GET /native/session/{id}/usage` |
| verification | `GET /native/session/{id}/verification`, `GET /native/session/{id}/tasks/{task_id}/verification` |
| evidence | `GET /native/evidence/{id}`, `POST /native/evidence/{id}/retrieve` |
| providers | `GET /native/providers` (registry view; `/models` catalog remains the fallback join) |
| terminals | `GET /native/terminals?session={id}`, `GET /native/session/{id}/terminal/events`, `POST /native/session/{id}/terminal` (session-owned spawn), `GET /pty/{pty_id}/output` (snapshot) |
| history / lifecycle | `GET /session/list`, start/stop/attach, `restart()` (session+cursor preserved), `reconnectStream()` (SSE cursor resume) |
| permissions | `GET /native/permissions?session={id}`, `POST /native/permission/reply` (strict `{session_id, permission_id, decision}`; typed 409 `conflict` / `permission_session_mismatch`) |

SSE frames carry `event:`, `id:` (journal sequence = resume cursor) and one
JSON `data:` line. Heartbeats are ignored but advance the cursor; oversized
frames are dropped loudly and skipped; reconnects resume from the last
delivered id with bounded exponential backoff.

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
   eleven parity families over canned frames (`task mode`, `agent tree with
   blockers/presentation/pixel identity`, `criterion proofs`, `permissions`,
   `terminal`, `review/tournament`, `evidence navigation`, `settings`,
   `provider selection`, `history`, `restart/reconnect`), then a raw-socket fake
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
