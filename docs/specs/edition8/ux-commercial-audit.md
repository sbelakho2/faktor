# Commercial UX audit — Edition 8 usage surfaces

Read-only heuristic audit of the commercial screens in both hosts, measured
with the real product HTML (VS Code) and the Swing panels (JetBrains) at
480 px, dark and high contrast, plus 200% text. No certification is claimed.

## Screens scored

VS Code S21, S27, S53, S54, S55, S14; JetBrains S21, S27, S53, S54, S55, S14,
plus the live Usage / Credits and Outcomes commercial panels.

Scores (1–5, first-time-user heuristics): clarity 4.96, density 4.72,
consistency 5.00, efficiency 4.96, recovery 5.00 across all 72 Rev8 screens.
The three usage screens intentionally trade a "controls first" score for the
specified layout order (served usage categories before read-only provenance
cards); the first served value is what a first-time user needs. The JetBrains
Edition 8 preview screens stay contract-only until a daemon binds them; the
live commercial data in that host is the Usage / Credits and Outcomes panels.

## Fixed defects (before → after)

| Defect | Evidence | After |
|---|---|---|
| Raw/lossy money | S54/S53/S27 showed `312500` for VCSC and truncated `$1.23`; the same value could read as $312,500 | Exact string arithmetic: `$0.3125`, `$1.234567`; no float conversion for u64 micro folds |
| B-02 re-entry error | The money-cap field pre-filled served micro `5000000` while Apply expected USD, staging a 1,000,000× cap | Field shows `5.00`; provenance shows `$5.00 · advanced 5,000,000µ$`; 0.00 keeps "no limit" |
| False unavailable routes | `Local routes Unavailable` / `Hosted routes Unavailable` while 30,000 and 12,000 tokens were served | Local inference `30,000 tokens`, Provider API `12,000 tokens`; missing categories remain unavailable, never zero |
| Missing S54 navigation | Spec says "Account Settings… opens S55"; neither host had the action (VS Code no link, Swing footer only Refresh/Back) | One secondary `Account Settings…` → S55 in both hosts; VS Code S54 stays read-only with no primary, Swing keeps its read Refresh primary |
| Missing S27 primary | Spec "Inspect Runs" had no control or target; control set is read-only sheets | One primary `Inspect Runs` screen link to S53 (the specified destination) |
| Duplicate commercial surfaces | S27/S53/S54 rendered the same ten rows; outcome metrics preceded usage categories on S54 | S54 categories first, S53 cohort cost first, S27 summary + calculation basis; no repeated efficiency/budget rows on read-only surfaces |
| Read-only shells before data | S54's two read-only cards pushed the first served value to y=506 (below the 480 fold) | Served state leads; first value at y=295; four rows visible at 480 px |
| Ambiguous JetBrains counts | Usage showed `2225 of 100000` with no locale grouping | `2,225 of 100,000`; exact values stay in advanced/copy surfaces |
| Cap breach by colour only | Over-cap managed spend was red but had no text consequence | Visible "Managed spend cap exceeded — new managed tasks are denied until the cap is raised."; token/credit variants have their own text |
| Grant credits jargon/one-step | Both hosts asked for a bare microUSD integer and sent immediately; a first-time user had to know the 1,000,000 scale | Amount accepts `$5.00`/`5.00` (USD) or `5000000µ$` (exact micro); bare integers are refused; a modal names the organization and exact `$5.0000 (5000000µ$)` before granting |
| Outcomes density | 15 padded metric cards (name + large value + basis card chrome) before any action; `$0.0000` for 5µ$; Refresh absent in the content state because Swing reparented the one button into the unavailable card | Compact three-line ledger rows, cohort header (`verified · money-settled · runs in the fold · scanned · window not served`), actions before the ledger, `under $0.0001 · 5µ$`, separate unavailable Refresh; first action moves above the 480 fold |

Measured first-screen deltas (480×900, dark):

| Screen | first served row | rows in first screen | visible buttons / primary | duplicate button labels |
|---|---|---|---|---|
| S54 | 506 → 295 px | 0 → 4 | 0/0 → 1/0 (Account Settings, read-only screen) | 0 → 0 |
| S53 | 627 → 259 px | 0 → 4 | 0/0 → 0/0 | 0 → 0 |
| S27 | 992 → 295 px | 0 → 4 | 0/0 → 1/1 (Inspect Runs) | 0 → 0 |
| S21 | 1884 px (controls-first by design) | 0 | 1/1 | 0; B-02 input 5000000 → 5.00; provenance `user · user` → `user` |

JetBrains (Swing, light host theme): Usage tokens `2225 of 100000` →
`2,225 of 100,000`; at 240 px the grouped number stays on one line (no
`100,`/`000` split); the cap-exceeded consequence is visible text, not only
red status. Outcomes: first action `Explain metric` at y=123 (before: below
480), 15 per-metric cards → 0 nested cards in one ledger, cohort header added,
Refresh present in the content state; 200% and HC behavior for the Edition 8
commercial screens is captured separately (S54 200% + HC, S21 HC).

## Typed spec gaps (not improvised)

- `GAP-COMM-01` — Plans/pricing comparison: no screen, control ID, plan
  catalogue route or price field in `screens.md` / `controls.json`; S54 only
  exposes a subscription quota. No comparison UI was invented.
- `GAP-COMM-02` — S53 `Open Runs` and S53/S27 `Export…`: no control ID, host
  command or target is specified. S27 `Inspect Runs → S53` is implemented from
  the Navigation line; the other two remain unnamed.
- `GAP-COMM-03` — Trial/proration/upgrade/downgrade/cancel: no Edition 8
  controls. Both hosts expose the live panel's Change plan…/Cancel
  subscription… flow, which takes a raw plan id and (trialing) an
  epoch-millisecond end, and reports proration only after apply. A price/
  proration/effective-date preview needs a plan catalogue and a rule, so it is
  recorded, not fabricated.
- `GAP-COMM-04` — Invoices/receipts/downloads: `GET /native/billing/overview`
  types invoices `unavailable` with a cause; no screen/control exists.
- `GAP-COMM-05` — Payment methods/status: typed `unavailable`; no surface.
- `GAP-COMM-06` — Organisation/seat management: typed `unavailable`; S55 is a
  machine-scoped optional account screen, not seat administration.
- `GAP-COMM-07` — Billing settings/currency/tax: no Edition 8 screen or
  controls; both capabilities are typed `unavailable`.
- `GAP-COMM-08` — Approval-related commercial actions: S14 approves tool
  operations only; it has no spend, credit, seat or plan effect, and no
  commercial action is routed through it. Plan change/cancel use a host modal.
- `GAP-COMM-09` — Current plan/status on S54: `/native/entitlements` serves
  quota and status but no plan id or price, so S54 can show
  `unlimited · active` only. The live Usage panel reads
  `/native/billing/overview` for the plan; a plan binding for S54 is not in
  the spec.

## Gates

`cargo fmt --check` and `cargo check --workspace` pass on the shared tree.
VS Code: `npm run build`; selftest 284/284; Edition 8 acceptance 40/40;
visual check 1656 cells / 0 failures; render check 32 screens; contrast and
optical checks 0 Faktor-drawn failures. JetBrains: `:frontend:compileKotlin`,
`:frontend:test`, and host matrix 1267/1267 against the re-pinned Edition 5
S27 (Outcomes) digest. The workspace `cargo test` run was not used as
evidence here (it exceeded the 15-minute local wall clock under concurrent
sibling load); no Rust source is changed by this audit. No certification is
claimed.
