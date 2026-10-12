# Faktor UI terminology

The product speaks three spaces — **Work**, **Inspect**, **History** — and one
vocabulary across VS Code, JetBrains and the CLI. Internal enum names and
persistence identifiers must never determine a visible label by default; the
table below is the shared translation.

| Internal concept | User-facing wording | Notes |
| --- | --- | --- |
| daemon / service process | **Faktor** / **Service** | `daemon` stays in logs, diagnostics and CLI service commands |
| service preflight / trust | **Service** status | stopped / missing / unverified / refused / starting / ready; the bare file's checksum wording is **Matches recorded checksum** / **No checksum record** / **Checksum mismatch** — never "signed" for the bare file |
| session | **Conversation** / work session | session ids are diagnostics |
| task / task run | **Run** | the current run is the primary status surface |
| task tree / DAG | **Plan** / run details | rendered as a plan, not a database dump |
| child agent | **Agent** | human role names first, ids in advanced details |
| blocker | **Needs attention** | one card per item, actions attached to that item |
| permission request | **Permission request** | ask about risk, target and reason — not numeric ids |
| tournament | **Compare approaches** | verification/review/cost, winner recommended |
| board | **Coordination** / agent activity | contextual under Agents, not a destination |
| evidence | **Evidence** | attached to a claim/criterion/change; raw selectors under Advanced |
| candidate change review | **Changes** / **Review change** | a change opens the host editor on an immutable served basis; when no immutable basis is served the surface says **Diff unavailable** and never compares live files |
| verification record | **Verification** | criteria, checks and reviewer in one place |
| proof basis | advanced verification details | hashes and bases stay reachable |
| completion contract | **Finish actions** | "Finish when done": leave/commit/push/PR |
| run options | **Task options** | composer disclosure in VS Code, bounded scrollable sheet in JetBrains: acceptance criteria, contract rows, finish level, per-run budget, attachments |
| mutation mode / shadow | **Isolated workspace** / advanced execution mode | `shadow` is diagnostics vocabulary |
| provider | **Provider** | only when a choice matters (model picker) |
| stream / cursor | **Live updates** / diagnostics | raw cursors never appear in normal copy |
| outcome analytics | **Outcomes** | run-outcome ledger metrics with definitions published; never public prices or invented savings, and an unavailable read shows no monetary value |
| usage micro-units | normal currency and tokens | exact micro accounting under Details |
| worktree id, snapshot id, proof hash | diagnostics only | reachable by drilling down, never headline copy |

## Where this is enforced

- VS Code: command taxonomy and the three-section settings hierarchy
  (**Faktor** run defaults / **Faktor: Advanced** machine service settings /
  **Faktor: Control Plane** optional coordinates) are pinned by
  `apps/vscode/scripts/selftest.mjs`; the webview ships a visible Work /
  Inspect / History switcher — Work carries the conversation, current run and
  ONE composer with the **Task options** disclosure, Inspect carries the
  Overview / Plan / Changes / Verification / Agents subviews, and History is
  a destination of its own with a host `faktor.switchSession` action and
  **Outcomes** beside the conversation list. Diagnostics carries the service,
  readiness and trust state plus guidance-only sandbox setup.
- JetBrains: the tool window exposes Work / Inspect / History clusters
  (`FaktorChatPanel`) with one selected detail surface per Inspect tab, a
  Task/Ask mode control, Work attention approvals, a four-level finish
  selector (Local / Commit / Push / PR mapped explicitly to the completion
  contract booleans), **Outcomes** beside the History list, and a platform
  application Configurable for settings; the host matrix renders every panel.
- CLI: `faktor --help` groups Daily, Service and Admin commands and opens
  with the product statement.
- README: the landing section leads with what Faktor does and a quick start;
  architecture stays below.

Adding a new user-visible surface means adding its row here (or reusing an
existing one) and wiring its label through the client layer — never printing
an internal identifier as the label.
