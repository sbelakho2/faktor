# IDE clients (Faktor-owned)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../docs/assets/brand/faktor-lockup-accent.svg">
  <img src="../docs/assets/brand/faktor-lockup-ink.svg" alt="Faktor" width="300">
</picture>

- `vscode/` — the Faktor VS Code extension: `src/` is the extension host,
  `media/chat.js` + `chat.css` + `composer-state.js` are the hand-written
  chat panel served by `src/webview.ts` (local-resource-only CSP, nonce,
  text-only rendering). No vendored UI bundle, bridge or staging step
  exists. The extension only launches the Faktor daemon
  (`faktor-cli serve --port 0`) and speaks the Faktor Native Protocol.
- `jetbrains/` — Faktor-owned JetBrains Kotlin client (split-mode shared/
  frontend/backend); the process manager launches the Faktor binary and
  the Swing panels are the only renderer.

## Daemon discovery, settings scoping and the install layout

The daemon executable is resolved from MACHINE/APPLICATION-scoped inputs
only. A workspace or repository can never select, parameterize or execute
the daemon:

- Resolution order: the machine-scoped `faktor.binaryPath` setting (VS Code)
  or the application-level Faktor binary path (JetBrains, set by
  "Locate daemon…") → `FAKTOR_BIN` → the signed install layout
  (`~/.faktor/install/launcher`, or `<dataDir>/install` / `faktor.installRoot`
  when a machine-scoped data dir / install root is configured). There is NO
  `target/debug|release` lookup and no repository-relative resolution: a
  workspace `target/` build is never executed.
- VS Code: the daemon-affecting settings (`faktor.binaryPath`,
  `faktor.installRoot`, `faktor.extraArgs`, `faktor.dataDir`,
  `faktor.startupTimeoutMs`, the control-plane coordinates) are declared
  `"scope": "machine"`. A workspace/workspace-folder value is ignored AND
  surfaced as a typed `workspace_scoped_setting_refused` notice.
- JetBrains: the binary path is an application-level property (IDE config
  directory, `faktor.daemon.binaryPath`), never a project property.
- Trust: where a checksum record exists beside the resolved executable
  (`<binary>.sha256`), the daemon must match it or the start is refused. The
  install-layout launcher verifies the activated release digest itself.
  Without any record the host warns with the exact resolved path and
  requires an explicit confirmation before spawning.
- Missing daemon: the start refuses with an actionable error naming the
  install location and the "Locate daemon…" action (JetBrains shows the
  dialog; VS Code points at the machine-scoped `faktor.binaryPath` setting).
- The daemon starts ONLY on an explicit user action (VS Code: "Diagnostics:
  Start Service", the panel's start action, or a task submission; JetBrains:
  "Start service"). No activation/tool-window auto-start exists, and the
  resolved executable is shown before it is spawned.

## Control-plane endpoint

`faktor.controlPlaneEndpoint` defaults to EMPTY: there is no built-in cloud
endpoint, so control-plane calls stay disabled until an operator explicitly
configures an endpoint (machine scope) and signs in. An empty endpoint is
never substituted with a vendor default.
