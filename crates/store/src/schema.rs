//! `schema`: cohesive slice of the mechanically decomposed parent module.

#[cfg(test)]
use super::*;

pub(crate) const MIGRATIONS: &[&str] = &[
    // v1 — initial schema
    "CREATE TABLE IF NOT EXISTS workspace (
        id INTEGER PRIMARY KEY,
        root TEXT NOT NULL UNIQUE,
        created_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS session (
        id INTEGER PRIMARY KEY,
        workspace_id INTEGER NOT NULL REFERENCES workspace(id),
        title TEXT NOT NULL DEFAULT '',
        provider TEXT NOT NULL DEFAULT '',
        model TEXT NOT NULL DEFAULT '',
        state TEXT NOT NULL DEFAULT '\"idle\"',
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS event (
        seq INTEGER NOT NULL,
        session_id INTEGER NOT NULL REFERENCES session(id),
        op_id INTEGER,
        kind TEXT NOT NULL,
        state TEXT NOT NULL,
        ts_ms INTEGER NOT NULL,
        payload TEXT,
        PRIMARY KEY (session_id, seq)
     );
     CREATE TABLE IF NOT EXISTS message (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        seq INTEGER NOT NULL,
        role TEXT NOT NULL,
        data TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        UNIQUE (session_id, seq)
     );
     CREATE TABLE IF NOT EXISTS part (
        id INTEGER PRIMARY KEY,
        message_id INTEGER NOT NULL REFERENCES message(id),
        kind TEXT NOT NULL,
        data TEXT NOT NULL,
        created_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS task (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        ledger TEXT NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS tool_run (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        op_id INTEGER NOT NULL UNIQUE,
        tool TEXT NOT NULL,
        args TEXT NOT NULL,
        status TEXT NOT NULL,
        started_ms INTEGER NOT NULL,
        ended_ms INTEGER,
        effect_status TEXT NOT NULL,
        recovery TEXT NOT NULL,
        expected_hash TEXT
     );
     CREATE TABLE IF NOT EXISTS provider_call (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        op_id INTEGER NOT NULL,
        provider TEXT NOT NULL,
        model TEXT NOT NULL,
        started_ms INTEGER NOT NULL,
        ended_ms INTEGER,
        status TEXT NOT NULL,
        tokens_in INTEGER,
        tokens_out INTEGER,
        error TEXT
     );
     CREATE TABLE IF NOT EXISTS checkpoint (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        sequence INTEGER NOT NULL,
        path TEXT NOT NULL,
        before_hash TEXT NOT NULL,
        after_hash TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        restored_ms INTEGER
     );
     CREATE TABLE IF NOT EXISTS artifact (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        kind TEXT NOT NULL,
        cas_hash TEXT NOT NULL UNIQUE,
        summary TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        size INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS worktree (
        id INTEGER PRIMARY KEY,
        workspace_id INTEGER NOT NULL REFERENCES workspace(id),
        path TEXT NOT NULL UNIQUE,
        branch TEXT NOT NULL,
        active INTEGER NOT NULL DEFAULT 1
     );
     CREATE TABLE IF NOT EXISTS memory_fact (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        kind TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        updated_ms INTEGER NOT NULL,
        UNIQUE (session_id, kind, key)
     );
     CREATE TABLE IF NOT EXISTS compaction (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        before_tokens INTEGER NOT NULL,
        after_tokens INTEGER NOT NULL,
        target_tokens INTEGER NOT NULL,
        accepted INTEGER NOT NULL,
        strategy TEXT NOT NULL,
        created_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS permission (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        op_id INTEGER NOT NULL,
        capability TEXT NOT NULL,
        decision TEXT NOT NULL,
        resolved_ms INTEGER,
        expires_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_event_session_seq ON event(session_id, seq);
     CREATE INDEX IF NOT EXISTS idx_message_session_seq ON message(session_id, seq);
     CREATE INDEX IF NOT EXISTS idx_toolrun_session ON tool_run(session_id, status);
     CREATE INDEX IF NOT EXISTS idx_checkpoint_session ON checkpoint(session_id, sequence);",
    // v2 — session lifecycle (orthogonal to the turn state machine)
    "ALTER TABLE session ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'open';",
    // v3 — checkpoint rows carry the CAS hash of the AFTER-content blob, so
    // unrevert (redo) and diff can reconstruct what the edit wrote. NULL on
    // pre-v3 rows: those checkpoints refuse redo/diff honestly.
    "ALTER TABLE checkpoint ADD COLUMN after_cas_hash TEXT;",
    // v4 — durable per-session prompt queue with a FULL execution envelope
    // and a durable state machine (audit rounds 6+7): pending | claimed |
    // running | done | cancelled. The user conversation message is NOT
    // stored here — it is materialized at ADMISSION (after the preceding
    // turn's output) so conversation chronology is the insertion order.
    "CREATE TABLE IF NOT EXISTS prompt_queue (
        session_id INTEGER NOT NULL REFERENCES session(id),
        seq INTEGER NOT NULL,
        op_id INTEGER NOT NULL,
        message_seq INTEGER,
        delivered INTEGER NOT NULL DEFAULT 0,
        prompt TEXT NOT NULL DEFAULT '',
        files TEXT NOT NULL DEFAULT '[]',
        model TEXT,
        variant TEXT,
        agent TEXT,
        status TEXT NOT NULL DEFAULT 'pending',
        requested_at INTEGER NOT NULL DEFAULT 0,
        claimed_at INTEGER,
        completed_at INTEGER,
        PRIMARY KEY (session_id, seq)
     );",
    // v5 — durable loop signals (spec §28): repeated identical failing
    // tool calls across LOGICAL TURNS and daemon restarts are detected
    // from this table, never from memory.
    "CREATE TABLE IF NOT EXISTS loop_signal (
        session_id INTEGER NOT NULL REFERENCES session(id),
        key TEXT NOT NULL,
        count INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, key)
     );",
    // v6 — checkpoint rows carry per-side EXISTENCE flags. A hash alone
    // cannot distinguish a missing file from an empty one: before==after
    // ("no change") currently means the empty-file creation is skipped and
    // rollback of a missing→content checkpoint would recreate an empty file
    // instead of deleting. DEFAULT 1 keeps pre-v6 rows readable: old rows
    // were only recorded for real files (the caller had content on both
    // sides), so "hash present with no existence marker means exists:true".
    "ALTER TABLE checkpoint ADD COLUMN before_exists INTEGER NOT NULL DEFAULT 1;",
    "ALTER TABLE checkpoint ADD COLUMN after_exists INTEGER NOT NULL DEFAULT 1;",
    // v7 — exact per-turn operation identity + recovery descriptors.
    // `turn_record` fixes the durable identity of every ADMITTED logical
    // turn (op id, queue seq, prompt message, effective provider/model/
    // variant/tool mode, status) so crash recovery resumes the SAME turn
    // with the SAME recorded envelope instead of synthesizing an operation.
    // `tool_run` gains the crash-recovery machinery: the durable
    // `replay_descriptor` (the stored invocation an idempotent tool may be
    // re-executed from), the `attempt` counter (a replay is a new PHYSICAL
    // attempt of the SAME logical operation) and the `postcondition`
    // (workspace-write verification data computed from the actual bytes as
    // written — never from JSON-encoded args).
    "CREATE TABLE IF NOT EXISTS turn_record (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES session(id),
        turn_op_id INTEGER NOT NULL,
        queue_seq INTEGER,
        prompt_message_id INTEGER,
        effective_provider TEXT NOT NULL DEFAULT '',
        effective_model TEXT NOT NULL DEFAULT '',
        variant TEXT,
        tool_mode TEXT,
        started_at INTEGER NOT NULL,
        status TEXT NOT NULL,
        updated_ms INTEGER NOT NULL,
        UNIQUE (session_id, turn_op_id)
     );
     CREATE INDEX IF NOT EXISTS idx_turn_record_session_status ON turn_record(session_id, status);
     ALTER TABLE tool_run ADD COLUMN replay_descriptor TEXT;
     ALTER TABLE tool_run ADD COLUMN attempt INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE tool_run ADD COLUMN postcondition TEXT;",
    // v8 — durable worktree/task identity on sessions. Tool calls were
    // being handed fake identities (worktree 1/task 1) because the real
    // ones lived nowhere durable: the session row now records them, so the
    // agent runtime builds `ToolRunCtx.identity` from the session row and
    // every replay descriptor / postcondition rides the SAME ids. DEFAULT 1
    // preserves existing rows: 1/1 is the documented STANDALONE session
    // identity (no worktree/task adopted); WorktreeManager-created
    // worktrees adopt their sessions deliberately afterwards.
    // (This block is array index 8, i.e. schema target 9: the v6 checkpoint
    // block spans two array entries before it.)
    "ALTER TABLE session ADD COLUMN worktree_id INTEGER NOT NULL DEFAULT 1;
     ALTER TABLE session ADD COLUMN task_id INTEGER NOT NULL DEFAULT 1;",
    // v9 — durable op-id sequence (schema target 10; array index 9). Op ids
    // used to be `now_ms + in-memory counter`: a daemon restart inside the
    // same millisecond (or after a backward clock jump) silently reused ids
    // that crash recovery still treats as live operations. The manager now
    // reserves RANGES from this table instead. `session_scope` is the scope
    // key: 0 is the ONE global sequence shared by every session (op ids are
    // globally unique — `tool_run.op_id` is a UNIQUE column). The seed row is
    // inserted by `migrate()` (not here) because its value is derived from
    // the wall clock at migration time: see `op_id_seq_seed`.
    "CREATE TABLE IF NOT EXISTS op_id_seq (
        session_scope INTEGER PRIMARY KEY CHECK (session_scope = 0),
        next_value INTEGER NOT NULL
     );",
    // v10 — first-class durable Task rows (schema target 11; array index
    // 10). Audit 25: no typed durable Task existed; the one-row-per-session
    // JSON ledger blob (the v1 `task` table) could not express task state,
    // bounded goal/criteria/plan or a durable budget. The typed table takes
    // the `task` name; the legacy ledger keeps its exact rows under its own
    // name `task_ledger` (get/put_task_ledger follow it; no data moves, the
    // rename is structural only — nothing references the old table).
    "ALTER TABLE task RENAME TO task_ledger;
     CREATE TABLE IF NOT EXISTS task (
        task_id INTEGER NOT NULL,
        session_id INTEGER NOT NULL REFERENCES session(id),
        goal TEXT NOT NULL,
        acceptance_criteria TEXT NOT NULL,
        plan TEXT NOT NULL,
        max_tokens INTEGER,
        max_turns INTEGER,
        spent_tokens INTEGER NOT NULL DEFAULT 0,
        spent_turns INTEGER NOT NULL DEFAULT 0,
        state TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, task_id)
     );
      CREATE INDEX IF NOT EXISTS idx_task_session_updated ON task(session_id, updated_ms);",
    // v11 — journal payload schema versions + the typed durable session
    // ledger (audits 27 / 71-72; schema target 12, array index 11). Every
    // `event` payload row now carries the payload schema version that wrote
    // it (readers refuse unknown versions loudly instead of misreading a
    // future shape). The opaque per-session ledger JSON blob stays untouched
    // (`task_ledger`); the RICH ledger is a typed, versioned, append-only
    // entry stream (`ledger_entry`) with a per-session materialized head
    // checkpoint (`ledger_head`) that compaction rewrites atomically with
    // the entry deletion it summarizes. The session layer computes the
    // never-FIFO-evict durability watermark: entries are deleted only below
    // it and the last GoalSet/CriteriaSet/Decision and every unresolved
    // BlockerOpened survive every compaction in code, never by accident.
    "ALTER TABLE event ADD COLUMN payload_ver INTEGER NOT NULL DEFAULT 1;
     CREATE TABLE IF NOT EXISTS ledger_entry (
        session_id INTEGER NOT NULL REFERENCES session(id),
        seq INTEGER NOT NULL,
        entry_type TEXT NOT NULL,
        schema_ver INTEGER NOT NULL DEFAULT 1,
        payload TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, seq)
     );
     CREATE INDEX IF NOT EXISTS idx_ledger_entry_session_seq ON ledger_entry(session_id, seq);
     CREATE INDEX IF NOT EXISTS idx_ledger_entry_session_type ON ledger_entry(session_id, entry_type);
     CREATE TABLE IF NOT EXISTS ledger_head (
        session_id INTEGER PRIMARY KEY REFERENCES session(id),
        head_json TEXT NOT NULL,
        checkpoint_seq INTEGER NOT NULL,
        schema_ver INTEGER NOT NULL DEFAULT 1,
        updated_ms INTEGER NOT NULL
     );",
    // v12 — durable per-workspace repository-index state machine rows
    // (schema target 13; array index 12; audits 30/64). The real
    // IndexService (faktor-index) persists its WorkspaceIndexState machine
    // here, one row per workspace, with an append-only transition journal.
    // `state_json` is opaque to this crate (protocol-agnostic, parsed by
    // the index layer); the `generation` column is the numeric generation
    // that row names (0 for NotStarted). Every transition updates the row
    // AND appends one journal row in the SAME transaction, so a daemon
    // restart resumes exactly the generation the crashed process left.
    "CREATE TABLE IF NOT EXISTS index_state (
        workspace_id INTEGER PRIMARY KEY REFERENCES workspace(id),
        state_json TEXT NOT NULL,
        generation INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS index_state_log (
        id INTEGER PRIMARY KEY,
        workspace_id INTEGER NOT NULL REFERENCES workspace(id),
        kind TEXT NOT NULL,
        state_json TEXT NOT NULL,
        generation INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_index_state_log_ws ON index_state_log(workspace_id, id);",
    // v13 — measurable prefix-cache stability (audits 65-66; schema target
    // 14; array index 13). Provider-side prompt caches turn a STABLE prompt
    // prefix into cheaper calls; a session that rewrites/reorders its
    // prefix every turn silently pays uncached prices. These ADDITIVE
    // provider_call columns persist one prefix observation per settled
    // usage row: `prompt_prefix_hash` (digest of the exact cacheable-prefix
    // byte string the caller sent, 32-byte BLOB), `prompt_tokens` (that
    // prefix's token count) and `prefix_stability` (the row's per-turn
    // stability in [0, 1], NULL until the settlement site fills it). Every
    // column is NULL-able with NO default: pre-v13 rows honestly read as
    // "no prefix observation recorded" and the routing layer never guesses.
    "ALTER TABLE provider_call ADD COLUMN prompt_prefix_hash BLOB;
     ALTER TABLE provider_call ADD COLUMN prompt_tokens INTEGER;
     ALTER TABLE provider_call ADD COLUMN prefix_stability REAL;",
    // v14 — task revisions + the first-class VerificationRecord (audit
    // P0-7/P0-8; schema target 15; array index 14). The typed task table
    // gains the per-row monotonic `revision` counter (DEFAULT 1 backfills
    // pre-v14 rows: every existing row was written once, so revision 1 is
    // the honest baseline) and the `verification_record` table becomes the
    // durable completion proof: immutable except the ONE CAS finalize
    // `Running -> Passed|Failed`. JSON columns follow the task-row style
    // (protocol-agnostic TEXT parsed fallibly on read); sizes are bounded by
    // the session layer before any write.
    "ALTER TABLE task ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
     CREATE TABLE IF NOT EXISTS verification_record (
        id INTEGER PRIMARY KEY,
        task_id INTEGER NOT NULL,
        revision INTEGER NOT NULL,
        workspace_id INTEGER NOT NULL,
        worktree_id INTEGER NOT NULL,
        tree_hash TEXT,
        criteria_json TEXT NOT NULL,
        checks_json TEXT NOT NULL,
        changed_files_json TEXT NOT NULL,
        unrelated_changes_json TEXT NOT NULL,
        reviewer_json TEXT,
        status TEXT NOT NULL,
        started_ms INTEGER NOT NULL,
        completed_ms INTEGER
     );
     CREATE INDEX IF NOT EXISTS idx_verification_record_task
        ON verification_record(task_id, id);",
    // v15 — the durable cost ledger (P0-6/12; schema target 16; array index
    // 15). The typed `task` row gains the MONETARY budget envelope columns:
    // `max_cost_micro` (NULL = unlimited; the token/turn caps stay where
    // they are) and `spent_cost_micro` (the durable settled-spend total).
    // These columns are READ-ONLY through the task machine: no generic task
    // upsert touches them (upsert_task enumerates its columns), the cost
    // ledger's own store section is their ONLY writer, and they are never
    // patched through `TaskBudget` — the ledger settles them in the same
    // transaction that closes a reservation, so the row and its
    // reservations can never disagree.
    //
    // `cost_reservation` records one attempt to spend task money, keyed by
    // its AUTOINCREMENT id (monotonic across daemon restarts, so a
    // reservation id is never reused after a crash). Every reservation
    // starts OPEN; a settlement (OPEN -> SETTLED) records the locally
    // calculated cost (`provider_cost_micro`), the provider-reported cost
    // when the usage frame carried one (`provider_reported_micro`), and the
    // routing decision's JSON when one produced the call
    // (`route_decision_json`, bounded by the session layer before the
    // write). A refund (OPEN -> REFUNDED) releases the reservation without
    // spending; crash recovery marks every surviving OPEN row ABANDONED
    // (the op never settled, so its prediction is never counted as spent).
    "ALTER TABLE task ADD COLUMN max_cost_micro INTEGER;
     ALTER TABLE task ADD COLUMN spent_cost_micro INTEGER NOT NULL DEFAULT 0;
     CREATE TABLE IF NOT EXISTS cost_reservation (
        reservation_id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id INTEGER NOT NULL,
        task_id INTEGER NOT NULL,
        op_id INTEGER NOT NULL,
        predicted_micro INTEGER NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('open', 'settled', 'refunded', 'abandoned')),
        created_ms INTEGER NOT NULL,
        settled_ms INTEGER,
        provider_cost_micro INTEGER,
        provider_reported_micro INTEGER,
        route_decision_json TEXT
     );
     CREATE INDEX IF NOT EXISTS idx_cost_reservation_session_task_status
        ON cost_reservation(session_id, task_id, status);",
    // v16 — spend-truth settlement + UNCERTAIN semantics (P0-1 settlement
    // half + P0-2; schema target 17; array index 16). The v15 crash rule
    // (OPEN -> ABANDONED, charged $0) undercounted spend when the crash hit
    // BETWEEN provider billing and the local settle: the provider may have
    // billed and the ledger counted zero. The v15 state `abandoned` is
    // RENAMED to `uncertain` — a reservation a crashed daemon may have
    // dispatched — and the table gains the durable dispatch marker
    // (`dispatched_ms`, NULL = dispatch never provably began; written
    // immediately before the provider transport call, so recovery can tell
    // "never dispatched -> refund" from "may have dispatched -> uncertain")
    // and the immutable route-time price capture (`pricing_snapshot_json`)
    // settlement prices usage against. Recovery code paths (not this
    // migration) split OPEN rows on that marker; this migration only
    // rebuilds the table: the CHECK swaps `abandoned` for `uncertain`, every
    // legacy `abandoned` row becomes `uncertain` (its prediction keeps
    // consuming the reserved amount), and pre-v17 rows read as
    // never-dispatched and unpriced. Column order in the INSERT is explicit
    // (the table is rebuilt, not altered), and the AUTOINCREMENT sequence is
    // re-seeded to the surviving max id by the explicit-id insert.
    "ALTER TABLE cost_reservation RENAME TO cost_reservation_v15;
     CREATE TABLE IF NOT EXISTS cost_reservation (
        reservation_id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id INTEGER NOT NULL,
        task_id INTEGER NOT NULL,
        op_id INTEGER NOT NULL,
        predicted_micro INTEGER NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('open', 'settled', 'refunded', 'uncertain')),
        created_ms INTEGER NOT NULL,
        settled_ms INTEGER,
        dispatched_ms INTEGER,
        pricing_snapshot_json TEXT,
        provider_cost_micro INTEGER,
        provider_reported_micro INTEGER,
        route_decision_json TEXT
     );
     INSERT INTO cost_reservation (
        reservation_id, session_id, task_id, op_id, predicted_micro, status,
        created_ms, settled_ms, dispatched_ms, pricing_snapshot_json,
        provider_cost_micro, provider_reported_micro, route_decision_json)
     SELECT reservation_id, session_id, task_id, op_id, predicted_micro,
            CASE status WHEN 'abandoned' THEN 'uncertain' ELSE status END,
            created_ms, settled_ms, NULL, NULL, provider_cost_micro,
            provider_reported_micro, route_decision_json
     FROM cost_reservation_v15;
     DROP TABLE cost_reservation_v15;
     CREATE INDEX IF NOT EXISTS idx_cost_reservation_session_task_status
        ON cost_reservation(session_id, task_id, status);",
    // v17 — attempt-identity accounting (audit Phase-1 items D/E/F + part of
    // G; schema target 18; array index 17). Every physical network attempt
    // now gets a fresh durable global OpId and the ledger rows key by that
    // ATTEMPT id instead of the shared turn/model-call op. The table is
    // rebuilt with the attempt/parent/delivery accounting columns and the
    // pre-dispatch state vocabulary: the v15/v16 `open` state is renamed
    // `reserved`, and `dispatched` becomes a REAL state (a reservation moves
    // reserved -> dispatched when its durable `dispatched_ms` marker is
    // written), so refund-after-dispatch is impossible at the SQL level:
    // a refund's guarded UPDATE (`status IN ('reserved','open') AND
    // dispatched_ms IS NULL`) changes zero rows on any dispatched/settled/
    // refunded/uncertain reservation, and the store refuses loudly instead
    // of freeing money. Legacy maps are lossless: `open` -> `reserved`
    // (carrying any v16 `dispatched_ms` marker — a v16 crash could leave
    // `open` + marker, which recovery still reads as may-have-dispatched),
    // `parent_op_id` is backfilled from `op_id` (the shared logical op every
    // legacy row keyed by), the reserve-time estimate is backfilled into
    // `estimated_cost_micro`, the provider-reported amount into
    // `provider_reported_cost_micro`, and `settled_cost_micro` /
    // `cost_basis` / `delivery_state` / `failure_reason_code` /
    // `request_id` are NULL (pre-v17 settlements never recorded which of the
    // two amount columns was folded, so an honest NULL beats a guessed
    // number). `provider_call` gains the same attempt identity columns
    // (NULL on legacy rows — each was its op's only physical attempt;
    // `parent_model_call_op_id` is backfilled from `op_id`).
    "ALTER TABLE cost_reservation RENAME TO cost_reservation_v16;
     CREATE TABLE IF NOT EXISTS cost_reservation (
        reservation_id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id INTEGER NOT NULL,
        task_id INTEGER NOT NULL,
        op_id INTEGER NOT NULL,
        attempt_op_id INTEGER,
        parent_op_id INTEGER,
        predicted_micro INTEGER NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('reserved', 'dispatched', 'settled', 'refunded', 'uncertain')),
        created_ms INTEGER NOT NULL,
        settled_ms INTEGER,
        dispatched_ms INTEGER,
        pricing_snapshot_json TEXT,
        provider_cost_micro INTEGER,
        provider_reported_micro INTEGER,
        route_decision_json TEXT,
        request_id TEXT,
        delivery_state TEXT,
        failure_reason_code TEXT,
        cost_basis TEXT,
        provider_reported_cost_micro INTEGER,
        estimated_cost_micro INTEGER,
        settled_cost_micro INTEGER
     );
     INSERT INTO cost_reservation (
        reservation_id, session_id, task_id, op_id, attempt_op_id,
        parent_op_id, predicted_micro, status, created_ms, settled_ms,
        dispatched_ms, pricing_snapshot_json, provider_cost_micro,
        provider_reported_micro, route_decision_json, request_id,
        delivery_state, failure_reason_code, cost_basis,
        provider_reported_cost_micro, estimated_cost_micro,
        settled_cost_micro)
     SELECT reservation_id, session_id, task_id, op_id, NULL, op_id,
            predicted_micro,
            CASE status WHEN 'open' THEN 'reserved' ELSE status END,
            created_ms, settled_ms, dispatched_ms, pricing_snapshot_json,
            provider_cost_micro, provider_reported_micro, route_decision_json,
            NULL, NULL, NULL, NULL, provider_reported_micro, predicted_micro,
            NULL
     FROM cost_reservation_v16;
     DROP TABLE cost_reservation_v16;
     CREATE INDEX IF NOT EXISTS idx_cost_reservation_session_task_status
        ON cost_reservation(session_id, task_id, status);
     CREATE INDEX IF NOT EXISTS idx_cost_reservation_attempt_op
        ON cost_reservation(attempt_op_id);
     ALTER TABLE provider_call ADD COLUMN attempt_op_id INTEGER;
     ALTER TABLE provider_call ADD COLUMN parent_model_call_op_id INTEGER;
     ALTER TABLE provider_call ADD COLUMN attempt_ordinal INTEGER;
     ALTER TABLE provider_call ADD COLUMN reservation_id INTEGER;
     UPDATE provider_call SET parent_model_call_op_id = op_id;
     CREATE INDEX IF NOT EXISTS idx_provider_call_session_attempt
        ON provider_call(session_id, attempt_op_id);",
    // v18 — verified-outcome learning (audit items 13/14/L; schema target
    // 19; array index 18). ONE new table, no other table changes: the
    // durable per-key verified-outcome projection. Rows key
    // (provider, model, phase, task_class, risk_bucket) and hold the five
    // accumulators with the router registry's invariant locked at the SQL
    // level (`sample_count = successes_first_pass + failures_first_pass`,
    // every column non-negative). Samples enter only through
    // `Store::model_outcome_stats_append` (a transactional
    // read-modify-write); the PK column order doubles as the per-phase
    // consult index (`provider, model, phase` prefix scans).
    "CREATE TABLE IF NOT EXISTS model_outcome_stats (
        provider TEXT NOT NULL,
        model TEXT NOT NULL,
        phase TEXT NOT NULL,
        task_class TEXT NOT NULL,
        risk_bucket TEXT NOT NULL,
        successes_first_pass INTEGER NOT NULL DEFAULT 0 CHECK (successes_first_pass >= 0),
        failures_first_pass INTEGER NOT NULL DEFAULT 0 CHECK (failures_first_pass >= 0),
        rework_cost_micro_sum INTEGER NOT NULL DEFAULT 0 CHECK (rework_cost_micro_sum >= 0),
        rework_turns_sum INTEGER NOT NULL DEFAULT 0 CHECK (rework_turns_sum >= 0),
        sample_count INTEGER NOT NULL DEFAULT 0 CHECK (sample_count >= 0),
        updated_ms INTEGER NOT NULL,
        CHECK (sample_count = successes_first_pass + failures_first_pass),
        PRIMARY KEY (provider, model, phase, task_class, risk_bucket)
     ) WITHOUT ROWID;",
    // v19 — per-call prompt segment observations (audits 45/82; schema
    // target 20; array index 19). The v13 prefix row persisted only the
    // binary digest of the cacheable prefix, so the routing layer could not
    // recover WHICH section of the prefix changed and approximated coverage
    // with the documented digest/growth-ratio pair rule. This ADDITIVE
    // nullable column persists the exact per-call `faktor_context`
    // `PrefixObservation` JSON the runtime measures at the settlement site
    // (ordered segment digests + token counts + observed cache reads), so
    // the router can compute the TRUE longest stable leading prefix and
    // price cache economics from it. NULL on pre-v19 rows: those rows keep
    // routing BYTE-IDENTICALLY on the binary pair rule — a missing
    // observation is never guessed. The store validates the payload's
    // strict shape and bounds on write AND read (a corrupt injected row is
    // a typed `Malformed`, never a silent fallback).
    "ALTER TABLE provider_call ADD COLUMN prefix_segments_json TEXT;",
    // v20 — verification environment fingerprint + candidate-proof reference
    // (audits 94/116/117; schema target 21; array index 20). Two ADDITIVE
    // nullable columns on `verification_record`: the bounded environment
    // fingerprint JSON the verification ran under, and the compact
    // candidate-proof reference (task revision, manifest aggregates, cheap
    // evidence folds, accounting digest). NULL on pre-v20 rows — a legacy
    // record honestly reads as "no fingerprint/no candidate ref recorded",
    // never a guessed value; the session layer owns the payload bounds and
    // parses the columns loudly on read.
    "ALTER TABLE verification_record ADD COLUMN environment_fingerprint_json TEXT;
     ALTER TABLE verification_record ADD COLUMN candidate_proof_ref_json TEXT;",
    // v21 — the durable evidence authority (efficiency audit: evidence/CCR
    // was designed but not the product's evidence store; schema target 22;
    // array index 21). ONE new table, no other table changes: every evidence
    // envelope the production context pipeline produces is durably recorded
    // here with its scope (session/workspace/task), kind, revision,
    // provenance/compressibility/compression/retrieval/compact JSON, the CAS
    // digest of its backing bytes and its capture completeness. `id` is
    // `INTEGER PRIMARY KEY AUTOINCREMENT`: evidence ids are GLOBALLY UNIQUE
    // across daemon restarts (never reused, even after row deletion) because
    // the backing content address must never alias a different envelope.
    // `backing_cas_hash` is deliberately NOT unique: identical backing bytes
    // deduplicate by CAS digest and may legitimately back many envelopes.
    // The scope index is the scoped-read path (`get_scoped` reads by id and
    // verifies scope; the session/workspace index is the listing path) and
    // the backing index is the "which evidence references this digest"
    // audit path — knowing the digest never grants a read; the evidence
    // layer's scope check still decides.
    "CREATE TABLE IF NOT EXISTS evidence (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id INTEGER NOT NULL REFERENCES session(id),
        workspace_id INTEGER NOT NULL REFERENCES workspace(id),
        task_id INTEGER,
        kind TEXT NOT NULL,
        revision INTEGER NOT NULL DEFAULT 1,
        provenance TEXT NOT NULL,
        compressibility TEXT NOT NULL,
        compression TEXT NOT NULL,
        retrieval TEXT NOT NULL,
        compact TEXT NOT NULL,
        backing_cas_hash TEXT,
        completeness TEXT NOT NULL,
        created_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_evidence_scope
        ON evidence(session_id, workspace_id, id);
     CREATE INDEX IF NOT EXISTS idx_evidence_session_task
        ON evidence(session_id, task_id, id);
     CREATE INDEX IF NOT EXISTS idx_evidence_backing_cas
        ON evidence(backing_cas_hash);",
    // v22 — durable verification attempts and jobs (audit P0-5/26; schema
    // target 23; array index 22). Four real tables replace the wave-9
    // `memory_fact` row hack. The identity is attempt-keyed everywhere:
    // `(session_id, task_id, attempt_op_id)` names one attempt,
    // `verification_attempt_changed_file` keys its changed files by
    // `(..., ordinal)`, and `verification_job` / `verification_job_result`
    // key every required check (inline AND background) by
    // `(session_id, task_id, attempt_op_id, check_id)`. Results from
    // attempt N are rows of attempt N: a newer attempt can never mutate
    // them, and the store refuses a late resolve of N once N+1 exists.
    // Foreign keys tie jobs to their committed attempt, so a torn begin is
    // impossible (the begin is one transaction). Caps mirrored from the
    // verification-record contract: changed files <= 4096, checks <= 256,
    // bounded argv.
    "CREATE TABLE IF NOT EXISTS verification_attempt (
        session_id INTEGER NOT NULL REFERENCES session(id),
        task_id INTEGER NOT NULL,
        attempt_op_id INTEGER NOT NULL,
        task_revision INTEGER NOT NULL,
        workspace_root TEXT NOT NULL,
        environment_fingerprint_json TEXT,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, task_id, attempt_op_id)
     );
     CREATE INDEX IF NOT EXISTS idx_verification_attempt_current
        ON verification_attempt(session_id, task_id, attempt_op_id);
     CREATE TABLE IF NOT EXISTS verification_attempt_changed_file (
        session_id INTEGER NOT NULL,
        task_id INTEGER NOT NULL,
        attempt_op_id INTEGER NOT NULL,
        ordinal INTEGER NOT NULL,
        path TEXT NOT NULL,
        PRIMARY KEY (session_id, task_id, attempt_op_id, ordinal),
        FOREIGN KEY (session_id, task_id, attempt_op_id)
            REFERENCES verification_attempt(session_id, task_id, attempt_op_id)
     );
     CREATE TABLE IF NOT EXISTS verification_job (
        session_id INTEGER NOT NULL,
        task_id INTEGER NOT NULL,
        attempt_op_id INTEGER NOT NULL,
        check_id TEXT NOT NULL,
        ordinal INTEGER NOT NULL,
        task_revision INTEGER NOT NULL,
        workspace_root TEXT NOT NULL,
        kind TEXT NOT NULL,
        command TEXT NOT NULL,
        program TEXT NOT NULL,
        args_json TEXT NOT NULL,
        spec_json TEXT,
        budget_ms INTEGER NOT NULL,
        inline_status TEXT,
        state TEXT NOT NULL,
        note TEXT,
        op_id INTEGER,
        environment_fingerprint_json TEXT,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        finished_ms INTEGER,
        PRIMARY KEY (session_id, task_id, attempt_op_id, check_id),
        FOREIGN KEY (session_id, task_id, attempt_op_id)
            REFERENCES verification_attempt(session_id, task_id, attempt_op_id)
     );
     CREATE INDEX IF NOT EXISTS idx_verification_job_open
        ON verification_job(session_id, task_id, state);
     CREATE TABLE IF NOT EXISTS verification_job_result (
        session_id INTEGER NOT NULL,
        task_id INTEGER NOT NULL,
        attempt_op_id INTEGER NOT NULL,
        check_id TEXT NOT NULL,
        result_json TEXT NOT NULL,
        finished_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, task_id, attempt_op_id, check_id),
        FOREIGN KEY (session_id, task_id, attempt_op_id, check_id)
            REFERENCES verification_job(session_id, task_id, attempt_op_id, check_id)
     );",
    // v23 — durable child-runtime blocker projection (schema target 24;
    // array index 23). The orchestrator registry row (a JSON memory fact)
    // carries the blocker fields for the graph/UI read-models; THIS typed
    // row is the bounded, queryable blocker truth keyed by the CHILD
    // session: state + blocker_kind/reason/dependency/resolution +
    // last_progress_ms. A NULL blocker triple means the child is not
    // blocked (a transition back to Running clears it). Strict text bounds
    // are enforced by the session layer BEFORE any write; existing rows
    // decode with NULL/None.
    "CREATE TABLE IF NOT EXISTS child_runtime (
        session_id INTEGER PRIMARY KEY REFERENCES session(id),
        child_id TEXT NOT NULL,
        state TEXT NOT NULL,
        blocker_kind TEXT,
        blocker_reason TEXT,
        blocker_dependency TEXT,
        blocker_resolution TEXT,
        last_progress_ms INTEGER,
        updated_ms INTEGER NOT NULL
     );",
    // v24 — durable binary/image attachments (schema target 25; array index
    // 24). ONE row per `(session_id, digest)`: the CAS address of the bytes
    // plus the typed metadata `AttachmentId { digest, mime, filename, size }`.
    // NOTE: the digest-only primary key was the attachment-identity bug the
    // v25 rebuild below fixes (identical bytes under a different MIME/
    // filename inherited the first row's metadata); this block stays as the
    // historical shape a pre-v25 database carries. The `task.attachments`
    // column carries the per-task typed list (bounded JSON; `'[]'` for every
    // pre-v24 row), SEPARATE from the workspace-relative `files`/`plan`
    // vocabulary.
    "CREATE TABLE IF NOT EXISTS attachment (
        session_id INTEGER NOT NULL REFERENCES session(id),
        digest TEXT NOT NULL,
        mime TEXT NOT NULL,
        filename TEXT,
        size INTEGER NOT NULL,
        PRIMARY KEY (session_id, digest)
     );
     ALTER TABLE task ADD COLUMN attachments TEXT NOT NULL DEFAULT '[]';",
    // v25 — attachment-reference identity (schema target 26; array index 25).
    // The v24 table keyed `(session_id, digest)`, so identical bytes under a
    // different MIME/filename silently inherited the FIRST row's metadata and
    // a fresh upload could never repair it. CAS content-addressing stays: the
    // blob is keyed by `digest` and one blob may back MANY attachment
    // references with distinct legitimate metadata. The row is now a
    // SURROGATE id plus a metadata-uniqueness index over
    // `(session_id, digest, mime, COALESCE(filename,''), size)` — the
    // COALESCE expression index is required because SQLite UNIQUE treats
    // NULLs as distinct, so two no-filename rows with identical metadata
    // would otherwise both live. Pre-v25 rows are preserved: the table is
    // rebuilt (create new, INSERT ... SELECT, drop old, rename) so every
    // existing reference survives with a new surrogate id.
    "ALTER TABLE attachment RENAME TO attachment_v24;
     CREATE TABLE attachment (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id INTEGER NOT NULL REFERENCES session(id),
        digest TEXT NOT NULL,
        mime TEXT NOT NULL,
        filename TEXT,
        size INTEGER NOT NULL
     );
     INSERT INTO attachment (session_id, digest, mime, filename, size)
        SELECT session_id, digest, mime, filename, size FROM attachment_v24;
     DROP TABLE attachment_v24;
     CREATE UNIQUE INDEX IF NOT EXISTS idx_attachment_reference
        ON attachment(session_id, digest, mime, COALESCE(filename, ''), size);
     CREATE INDEX IF NOT EXISTS idx_attachment_blob_order
        ON attachment(session_id, digest, id);",
];

/// Array index of the v9 block above (migration list position, not the
/// schema target — targets are 1-based).
pub(crate) const OP_ID_SEQ_MIGRATION_INDEX: usize = 9;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_and_reopen_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Store::open(dir.path(), true).unwrap();
            s.create_workspace("/tmp/ws").unwrap();
            s.create_session(WorkspaceId::new(1), "t", "ollama", "qwen3.8")
                .unwrap();
        }
        // Reopen: migrations must be a no-op and data must survive.
        let s = Store::open(dir.path(), true).unwrap();
        assert!(s.get_session(SessionId::new(1)).unwrap().is_some());
        assert_eq!(
            s.last_event_seq(SessionId::new(1)).unwrap().unwrap().raw(),
            1
        );
    }
}
