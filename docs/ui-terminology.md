# Faktor UI terminology

The product speaks three spaces — **Work**, **Inspect**, **History** — and one
vocabulary across VS Code, JetBrains and the CLI. Internal enum names and
persistence identifiers must never determine a visible label by default; the
table below is the shared translation.

| Internal concept | User-facing wording | Notes |
| --- | --- | --- |
| daemon / service process | **Faktor** / **Service** | `daemon` stays in logs, diagnostics and CLI service commands |
| session | **Conversation** / work session | session ids are diagnostics |
| task / task run | **Run** | the current run is the primary status surface |
| task tree / DAG | **Plan** / run details | rendered as a plan, not a database dump |
| child agent | **Agent** | human role names first, ids in advanced details |
| blocker | **Needs attention** | one card per item, actions attached to that item |
| permission request | **Permission request** | ask about risk, target and reason — not numeric ids |
| tournament | **Compare approaches** | verification/review/cost, winner recommended |
| board | **Coordination** / agent activity | contextual under Agents, not a destination |
| evidence | **Evidence** | attached to a claim/criterion/change; raw selectors under Advanced |
| verification record | **Verification** | criteria, checks and reviewer in one place |
| proof basis | advanced verification details | hashes and bases stay reachable |
| completion contract | **Finish actions** | "Finish when done": leave/commit/push/PR |
| mutation mode / shadow | **Isolated workspace** / advanced execution mode | `shadow` is diagnostics vocabulary |
| provider | **Provider** | only when a choice matters (model picker) |
| stream / cursor | **Live updates** / diagnostics | raw cursors never appear in normal copy |
| usage micro-units | normal currency and tokens | exact micro accounting under Details |
| worktree id, snapshot id, proof hash | diagnostics only | reachable by drilling down, never headline copy |

## Where this is enforced

- VS Code: command taxonomy and settings hierarchy are pinned by
  `apps/vscode/scripts/selftest.mjs`; the webview ships Work (conversation,
  current run, composer) with everything else behind **Inspect**.
- JetBrains: the tool window exposes Work / Inspect / History clusters
  (`FaktorChatPanel`), with the host matrix rendering every panel.
- CLI: `faktor --help` groups Daily, Service and Admin commands and opens
  with the product statement.
- README: the landing section leads with what Faktor does and a quick start;
  architecture stays below.

Adding a new user-visible surface means adding its row here (or reusing an
existing one) and wiring its label through the client layer — never printing
an internal identifier as the label.
