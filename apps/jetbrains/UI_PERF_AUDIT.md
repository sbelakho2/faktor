# JetBrains client live UI/UX performance audit

Scope: `apps/jetbrains` (native Swing client). Verification is the new
`EdtWorkHarness` (`--smoke EdtWork`), which drives the real pump, transcript
sink, keyed models, gate and panel wiring at production pressure:
10 000-token stream, 200 deltas/s, 500-message transcript, 1 000 rows.
Artifact: `target/certification/jetbrains-edt-harness.json`.

| Area | file:line | Per what | Realistic size | Verdict |
|---|---|---|---|---|
| Stream -> EDT | `FaktorChatPanel.kt:3670`, `UiWork.kt:158` | One enqueue per journal frame; one EDT flush per 40 ms, max 256 frames, 1 ms re-arm; hard 16 384-frame / 8 MiB queue bound drops oldest (metrics) | 10k frames @200/s | **fix** (was one `invokeLater` + refresh per frame) |
| Slow refresh fan-out | `FaktorChatPanel.kt:3738`, `:3749` | Messages/board/tree/agents/usage reads coalesced to one dirty-flag refresh per 250 ms, single-flight | 200 frames/s | **fix** (was 5 reads + JSON parses per frame) |
| SSE frame JSON parse | `NativeEventStream.kt` (`readFrame`/parse loop), `FaktorChatPanel.kt:3681` | One bounded frame validation per SSE frame (protocol authority); the panel no longer re-parses full message/board/tree/usage payloads per delta | ~2 KiB/frame @200/s | **must stay** (frame validation is the stream's security contract; the per-delta full-payload fan-out is removed) |
| Transcript append/layout | `FaktorChatPanel.kt:195`, `:4268`, `UiWork.kt:324` | One `Document.insertString` per flush; maintained char/line budget; no per-delta `text.length` scan | 500 messages, 10k tokens | **fix** (was one insert + full-text scan per delta) |
| Scroll pinning | `UiWork.kt:250`, `:282` | Follow only while at bottom; user scroll detaches; bottom return/jump resumes; one scrollbar move per flush | 4000-line window | **fix** |
| Task tree model | `TaskTreePanel.kt:291`, `:316`, `:363` | Keyed merge preserved; O(n) key index instead of per-row `keyPath`; `nodeChanged` on in-place label change; large-model mode | 1k rows | **fix** (was O(n²) capture + stale labels) |
| History list | `HistoryPanel.kt:174`, `:274` | Identical snapshot is a strict no-op; changed rows update in place; selection/scroll survive | 1k sessions | **fix** (was clear+addAll per refresh) |
| Agents roster | `AgentsPanel.kt:273`, `:295` | Idempotent snapshot guard; keyed combo sync; document text written only on change | 200 agents | **fix** |
| Board | `BoardPanel.kt:183`, `:191` | Human/advanced documents assigned only when changed | 100 posts/page | **fix** (was `text=` per refresh) |
| Diff/changes | `ChangesPanel.kt:200`, `:332` | Identical model no-op; keyed row reuse by path; conflict facts rebuilt only when changed | 1k changed files | **fix** (was full row/column reset per refresh) |
| Usage/outcomes/overview/verification/tournament | `UsagePanel.kt:91`, `OverviewPanel.kt:73`, `VerificationPanel.kt:71`, `OutcomesPanel.kt:643`, `TournamentPanel.kt:92` | Model equality guards; `UsagePanelModel.lines()` formatted once per model; candidates table no-op | 1k ledger rows | **fix** |
| Money formatting | `UiWork.kt:443`, `UsagePanel.kt:189`, `:453` | Bounded LRU keyed by exact `BigInteger` (+currency); percent keyed by (observed, limit); 4096-entry cap | 10k distinct micro-USD values | **fix** (was division/format per render) |
| Image work | `AttachmentsPanel.kt:974`, `:995`, `PixelAgents.kt:322` | No raster image is decoded/reloaded during render (sprites are procedural per child); UI paste/drop PNG encoding now runs on the daemon image worker, never the EDT; synchronous staging stays for callers that own bytes; worker is shut down with the panel | user paste/drop only | **fix** (EDT encode removed; no per-render image path to lazy-load) |
| Theme/HiDPI/reduced motion | `FaktorChatPanel.kt:552`, `PanelSupport.kt:325`, `PixelAgents.kt:77` | Live `updateUI` refreshes composer/transcript fonts; wrapped labels re-measure on the next layout pass; `PixelMotion` suppresses sprite timers under IDE presentation/reduced motion | 240/320/480/800 px, zoom | **must stay/fixed** (no animations to gate; timer already reduced-motion aware) |
| Keyboard/focus | `FaktorChatPanel.kt:612`, `:647`; `HistoryPanel.kt:126`; `AgentsPanel.kt:82` | Panel-level Escape (ancestor scope), composer Ctrl+Enter, session/history/agents Enter activation; approval cards deliberately never steal focus | keyboard-only host matrix | **fix** (Escape was focus-scoped to the panel only) |
| Accessibility | `UiWork.kt:741`; `FaktorChatPanel.kt:570`; `HistoryPanel.kt:124`; `BoardPanel.kt:95`; `ChangesPanel.kt:135`; `UsagePanel.kt:343`; `TerminalPanel.kt` init | Accessible names/descriptions on transcript, composer, chooser, runs, lists, tables, trees, forms, progress; routine model chunks no longer enter the live transcript announcement stream | 369 host-matrix checks | **fix** |
| Error/loading/empty | `BoardPanel.kt:309`; `HistoryPanel.kt:195`; `ChangesPanel.kt:332`; `UsagePanel.kt:520`; `AttachmentsPanel.kt:893` | Explicit unavailable/refused/empty copy preserved; no fabricated rows or values | hostile/absent routes | **must stay** (already explicit; verified by smoke) |
| Reconnect/replay | `FaktorChatPanel.kt:3681`, `:4313`; `NativeEventStream.kt` | Replayed/session-stale frames dropped by session + seq; recovery copy/cursor unchanged; per-session scope discards queued work on switch | cursor resume + replay | **fix** (duplicate frames were appended and re-announced) |
| Listener teardown | `FaktorChatPanel.kt:3778`, `UiWork.kt:704` | Shutdown detaches the service listener, stops pump/refresh timers, disposes the session scope, then stops streams/workers; session switch disposes/recreates the scope | every switch/shutdown | **fix** (listener + high-rate queue outlived the panel/session) |

## Harness evidence (before -> after)

- 10k tokens @200/s: 10 000 EDT tasks / 10 000 doc inserts / 10 000 refresh
  triggers -> 1 250 flushes (8.0x), 1 250 doc inserts, 200 slow refreshes.
- 10k burst: 40 bounded EDT batches (max 256), lossless and order-preserving;
  a hostile 150-frame burst into a 100-frame queue retains the newest 100 in
  order.
- Real pump burst: 10 000 frames offered at once drain through the EDT in 40
  flushes (max 256, 1 ms re-arm), 0 dropped, exact arrival order.
- 500-message transcript: 500 inserts -> 1 insert.
- 1k-row table: 1 001 reset events -> 0 for an identical snapshot, 1 for one
  changed row.
- Real 200 deltas/s: 200 offers -> 22 EDT flushes (9.1x).
- Panel wire: 10 000 `onEvent` calls do not touch the document off-flush;
  0 transcript inserts for routine chunks; 5 replayed frames dropped.
- Money: 1 000 first-pass misses, 1 000 second-pass hits, cache <=4096.
