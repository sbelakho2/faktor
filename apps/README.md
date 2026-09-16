# IDE clients (Faktor-owned)

- `vscode/` — the Faktor VS Code extension: `src/` is the extension host,
  `media/chat.js` + `chat.css` + `composer-state.js` are the hand-written
  chat panel served by `src/webview.ts` (local-resource-only CSP, nonce,
  text-only rendering). No vendored UI bundle, bridge or staging step
  exists. The extension only launches the Faktor daemon
  (`faktor-cli serve --port 0`) and speaks the Faktor Native Protocol.
- `jetbrains/` — Faktor-owned JetBrains Kotlin client (split-mode shared/
  frontend/backend); the process manager launches the Faktor binary and
  the Swing panels are the only renderer.
