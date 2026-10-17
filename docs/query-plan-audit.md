# SQL query-plan audit (store, cloud, scm, worker, updater, index, commerce, scheduler)

Method: 704 production SQL literals were enumerated with a string-literal scan over
the owned crates (tests excluded; schema/migration DDL included). Every hot read was
run through `EXPLAIN QUERY PLAN` against a seeded SQLite database at the audit sizes:
10k sessions/messages/tasks, 50k events, 4k permissions/memory facts, 20k provider
calls/ledger/audit rows, 10k reservations/tool runs/turn records/evidence/leases,
50k offers/cache rows, 20k SCM webhook receipts. Plans are pinned by permanent
`EXPLAIN` assertions (`crates/store/src/query_plan_tests.rs` and the per-crate
`query_plan_tests.rs` files). SQLite is the workspace's bundled rusqlite build.

Plan vocabulary: **SEARCH** = index range lookup (good); **SCAN** = full table or
partial-index walk; **TEMP** = temp B-tree for sort/distinct; a partial-index `SCAN`
is the optimal whole-partition walk, not a table scan.

## Migrations

| DB / crate | Change | Discipline |
|---|---|---|
| store (head, last entry) | drop duplicate `idx_event_session_seq`, `idx_message_session_seq`, replace `idx_task_session_updated`; add 13 indexes: part(message), permission(session,decision,id,expires), permission(decision,id), session(workspace,updated), task_ledger(session,updated), prompt_queue(status,session,seq), memory_fact(kind,session), memory_fact(updated), partial event(turn_completed), partial tool_run(running), provider_call(session,id), partial turn_record(active), task(session,created,task) | appended ladder entry, one transaction, restore point first, head/newer/negative refusal unchanged |
| cloud (CP ladder v7) | idempotency(created,key), approval(org,id), replace in-flight(org,ended) -> (org,started,id), audit(org,occurred,seq), report_period(org,first_seen,period) | same ladder discipline (backup, serialized opener, newer/negative refusal) |
| scm (v3) | repository(installation,id), webhook(received,delivery,event) | same |
| worker (v3) | lease(org,state,lease_id), lease(worker,state,lease_id) (replacements; no index-count growth) | same |
| updater (v4) | operation(status,created,id), operation(kind,status,created,id), operation(created,id), partial expression index on json_extract(payload,'$.idempotency_key') WHERE json_valid(payload) | same; partial predicate keeps corrupt JSON from bricking upgrade |
| commerce (v4) | replace offer(source,source_offer_id,observed,id), replace cache expression index on COALESCE(revalidated_at_ms,observed_at_ms), partial job(owner,digest,created,job_id) WHERE active, partial job(created,job_id) WHERE active+owned | same |

Upgrade tests: store previous-head replay + backup/restore plan check; cloud v7, scm v3,
worker v3, updater v4 (including a corrupt row), commerce v4. Negative/newer-version
refusals and serialized-opener tests remain the existing dynamic-head tests.

## Findings (hot statements)

| file:line | purpose | before plan | after plan | size | verdict |
|---|---|---|---|---|---|
| store/sessions.rs:536 | workspace session list | SCAN + TEMP | SEARCH `idx_session_workspace_updated` | 10k sessions | optimized (2.05 ms -> 1.36 ms) |
| store/sessions.rs:540 | global session list | SCAN + TEMP | SCAN + TEMP | 10k | must stay (full listing, no covering shape) |
| store/messages.rs:89 | message page | SEARCH unique autoindex | SEARCH unique autoindex (duplicate named index dropped) | 10k msgs | must stay |
| store/messages.rs:244 | parts of one message | SCAN part | SEARCH `idx_part_message` | 30k parts | optimized (340 us -> 2 us) |
| store/sessions.rs:1006/1029/1038 | permission expiry sweep | SCAN permission | SEARCH `idx_permission_session_pending` | 4k | optimized (47 us -> 2 us) |
| store/sessions.rs:1667/1672 | global pending count/list | SCAN permission | SEARCH covering `idx_permission_decision_id` | 4k | optimized |
| store/ledger.rs:924 | legacy task ledger read | SCAN + TEMP | SEARCH `idx_task_ledger_session_updated` | 10k | optimized (115 us -> 1 us) |
| store/ledger.rs:1609 | per-session FIFO queue head | SEARCH PK (session,seq) | same | 5k | must stay |
| store/ledger.rs:2015 | startup non-terminal queue scan | SCAN | SEARCH `idx_prompt_queue_status_session` | 5k | optimized |
| store/tasks.rs:2428 | turn-completed count | PK range + kind filter | SEARCH covering partial `idx_event_session_turn_completed` | 50k events | optimized |
| store/ledger.rs:2627 | newest evidence page | index range + TEMP sort on created_ms | SEARCH `idx_evidence_scope`, no sort (`ORDER BY id DESC`) | 10k | optimized + correctness fix |
| store/tasks.rs:2413 | task listing | index + TEMP sort | SEARCH `idx_task_session_created` | 10k | optimized |
| store/connection.rs:1931/1951 | open-time fact watermark | SCAN memory_fact | SEARCH covering `idx_memory_fact_updated` | 4k | optimized (71 us -> 1 us) |
| store/migration/legacy_import.rs:666 | per-open legacy session scan | SCAN covering unique | SEARCH `idx_memory_fact_kind_session` + TEMP DISTINCT | 4k | optimized (111 us -> 3 us; DISTINCT inherent) |
| store/tasks.rs:2848 | crash recovery: running tools | SCAN + TEMP | SCAN partial `idx_tool_run_running` | 10k | optimized (225 us -> 3 us) |
| store/tasks.rs:2895 | crash recovery: active turns | SCAN + TEMP | SCAN partial `idx_turn_record_active` | 10k | optimized (229 us -> 5 us) |
| store/tasks.rs:2551 | shell pre-manifest listing | session index + TEMP | same | per-session bounded | must stay (crash-only; partial index would block downgrade) |
| store/billing.rs:1187/1256 | provider prefix rows/stability | session index + TEMP | SEARCH `idx_provider_call_session_id` | 20k calls | optimized |
| store/tasks.rs:592 | verification jobs + result subquery | correlated PK lookups | same | <=256 checks/attempt | must stay (bounded correlated PK lookups) |
| store/tasks.rs:704/1955 | max attempt / open job probe | PK / `idx_verification_job_open` | same | bounded | must stay |
| store/billing.rs:1308/1338 | reservation status fold / orphans | scan + PK probes | same | doctor/recovery only | must stay |
| store/billing.rs:2484/2559 | reservation pages | `idx_cost_reservation_session_task_status` + sort | same | per task | must stay |
| store/attachments.rs:803/913 | attachment blob pages | unique/covering index | same | per digest | must stay |
| store/attachments.rs:935/947/962 | attachment/artifact GC | full scans | same | GC-only | must stay |
| cloud/store.rs:1530/1542/1559 | idempotency retention (open tick) | SCAN + TEMP | SEARCH `idx_cp_idempotency_created` | 20k | optimized |
| cloud/store.rs:2295 | approval NULL-status page | index + TEMP | SEARCH `idx_cp_approval_org_id` | 4k | optimized |
| cloud/billing_store.rs:1794 | in-flight listing | index + TEMP | SEARCH replaced `idx_billing_in_flight_org` | 5k | optimized |
| cloud/enterprise_store.rs:557 | audit retention boundary | org-seq index + filter | SEARCH `idx_ent_audit_org_occurred` | 20k | optimized |
| cloud/billing_store.rs:1865 | report-period listing | PK + TEMP | SEARCH `idx_billing_report_period_org_seen` | 2k | optimized |
| cloud/store.rs:1703..2475 | users/orgs/members/invites/sessions/approvals/idempotency by PK/unique | SEARCH | same | - | must stay |
| scm/store.rs:709 | repositories by installation | index + TEMP | SEARCH `idx_scm_repository_installation` | 5k | optimized |
| scm/store.rs:756 | webhook receipt listing | SCAN + TEMP | SCAN covering `idx_scm_webhook_received` | 20k | optimized |
| scm/store.rs:684/803/829 | repos by org/id, external op, rate limit | SEARCH | same | - | must stay |
| worker/store.rs:2107/2123 | live leases by org / worker | index + TEMP | SEARCH lease index with lease_id order | 10k leases | optimized |
| worker/store.rs:2266/2281/2570 | attempts, max attempt, journal page | PK / journal index | same | bounded | must stay |
| updater/store.rs:813/831/851 | operation list/running/latest | SCAN/TEMP | SEARCH ordered indexes | 10k | optimized |
| updater/store.rs:866 | idempotency by JSON key | SCAN (json_extract) | SEARCH expression index | 10k | optimized (JSON predicate) |
| updater/store.rs:800/882 | get / high-water | PK | same | - | must stay |
| commerce/store.rs:1146/1177 | latest offer by source / identity | index + TEMP (source path) | SEARCH `idx_offer_snapshot_offer` | 50k | optimized |
| commerce/store.rs:2030 | cache age purge (COALESCE) | SCAN (function predicate) | SEARCH expression `idx_search_cache_expiry` | 50k | optimized |
| commerce/store.rs:1754/1788 | active job lookup / FIFO | index + TEMP / SCAN + TEMP | SEARCH/SCAN partial active-job indexes | 10k jobs | optimized |
| commerce/store.rs:896/1277/1456/1598/1665/1855/1884 | identity, cache key, connector, egress, challenge, job item | SEARCH | same | - | must stay |
| store/index.rs:70/141/191 | index state row + journal page | PK / (workspace,id) | same | per workspace | must stay |
| scheduler | no persisted SQL | - | - | - | n/a |

## Correctness bug fixed

`Store::evidence_list_by_scope_newest` ordered by `created_ms DESC, id DESC` but used
`id < before` as its exclusive cursor. A clock step or an explicit-id import makes the
orders disagree, so a page could end at id 2 while id 3 (older timestamp) had not been
emitted; the next page then excluded it forever. The durable query now orders by the
monotonic id (matching the in-memory store and the cursor) and the scope index serves
it without a temp sort. Regression test:
`newest_evidence_pagination_never_skips_rows_when_timestamps_disagree`.

## Measurements (seeded sizes, best of five, bundled SQLite)

Reads, same connection before/after the index step (before drops the audited indexes
and recreates the predecessor duplicates):

| read | before | after |
|---|---|---|
| parts of one message | 340 us | 2 us |
| permission sweep | 47 us | 2 us |
| workspace session list (10k) | 2054 us | 1363 us |
| task ledger | 115 us | 1 us |
| newest evidence page | 13 us | 11 us |
| task list | 5 us | 4 us |
| memory MAX watermark | 71 us | 1 us |
| memory kind scan | 111 us | 3 us |
| global running tools | 225 us | 3 us |
| global active turns | 229 us | 5 us |
| provider prefix rows | 3 us | 3 us (plan fixed) |
| message page / turn count | 5 / 2 us | 5 / 2 us |

Write path (2000 x event+message+part+provider_call inserts plus one session update,
one transaction, best of three): 4861 us before -> 4957 us after (+2.0%). Event and
message inserts got cheaper (duplicate index dropped); part and provider_call pay one
new index each.

Residual: none. Index count is bounded: store net +10 (13 added, 3 duplicates
removed), cloud +4 (four added, one replaced in place), scm +2, worker 0 (2 replaced), updater +2, commerce +2.
