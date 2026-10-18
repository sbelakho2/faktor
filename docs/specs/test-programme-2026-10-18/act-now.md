# Act now — four immediate items (plus the 1-micro regression)

Report: p.3 "Act now"; build order step 1 (p.46). These are the items that
must land before any registry work. They are ordered by blast radius, not
effort.

## 1. Remove the two core dumps from history (p.3; row 19, p.7)

### Facts
- Paths (repo root):
  - `qemu_chrome_20261018-133409_41.core` — 89,522,176 bytes
  - `qemu_chrome_20261018-133409_43.core` — 89,534,464 bytes
- Commit that added them: `ec502901cf1d0b0318114f6d88340fb67f99e71e`
  ("Fixes", 2026-10-18). `git log --all --diff-filter=A -- '*.core'` and
  `git log --all --find-object=<blob>` both resolve only to this commit.
- Blob ids:
  - `c648467fd8783f070a9b01afa0fba1be0f0be126`
  - `502deb5d80def31950ffbd6d53a6b88da1af6a90`
- `.gitignore:15-32` ignores `core` and `core.*` plus source negatives; it
  does **not** ignore `*.core`.
- The report's pattern scan found no credentials in the dumps (p.3). Still
  run a secret scan before publishing the rewritten history; never print
  blob contents.

### Safe procedure (history rewrite)
Run from a clean clone/worktree. `Report.pdf` is untracked; move it outside
the repo or add it to a temporary exclude before rewriting.

```sh
# 0) Prove the starting point.
git status --short
git rev-parse HEAD
git ls-files 'qemu_chrome_20261018-133409_*.core'
git cat-file -s c648467fd8783f070a9b01afa0fba1be0f0be126
git cat-file -s 502deb5d80def31950ffbd6d53a6b88da1af6a90

# 1) Back up every ref and the full object graph.
git tag backup/pre-core-purge ec502901cf1d0b0318114f6d88340fb67f99e71e
git branch backup/pre-core-purge ec502901cf1d0b0318114f6d88340fb67f99e71e
git bundle create /tmp/faktor-pre-core-purge.bundle --all
sha256sum /tmp/faktor-pre-core-purge.bundle > /tmp/faktor-pre-core-purge.bundle.sha256

# 2) Rewrite all refs. Prefer git-filter-repo.
git filter-repo --invert-paths \
  --path qemu_chrome_20261018-133409_41.core \
  --path qemu_chrome_20261018-133409_43.core

# Fallback if git-filter-repo is unavailable:
# git filter-branch --force --index-filter \
#   'git rm --cached --ignore-unmatch \
#      qemu_chrome_20261018-133409_41.core \
#      qemu_chrome_20261018-133409_43.core' \
#   --prune-empty --tag-name-filter cat -- --all
```

```sh
# 3) Verify the object graph, not just the current tree.
git rev-list --objects --all | grep -E 'qemu_chrome.*\.core' && exit 1
git cat-file --batch-all-objects --batch-check='%(objectname) %(objecttype) %(objectsize)' \
  | grep -E '^(c648467fd8783f070a9b01afa0fba1be0f0be126|502deb5d80def31950ffbd6d53a6b88da1af6a90) ' && exit 1
git count-objects -vH

# 4) Drop reflogs and unreachable objects so a fresh clone cannot fetch them.
git reflog expire --expire=now --all
git gc --prune=now --aggressive
```

```sh
# 5) Publish. Force-with-lease is mandatory; never plain --force.
git push --force-with-lease origin main
git push --force-with-lease --tags origin
# If other long-lived branches exist, rewrite and push them explicitly:
# git push --force-with-lease origin <branch>
# A mirror push rewrites every ref; use only after the backup bundle verifies:
# git push --force --mirror origin
```

Operational notes:
- Every collaborator must re-clone or run
  `git fetch origin && git reset --hard origin/main`; an old clone still has
  the blobs locally and can re-push them.
- Clear Woodpecker caches/artifacts that may retain the old packfiles. The
  repository history is the source of truth; caches are not.
- If the remote rejects the force-push (protected branch), temporarily lift
  the protection with an audit note, push, restore it, and record the
  operator/time in the release log.
- If the secret scan finds a credential in either blob, rotate it before
  publishing; the report says none was found, but the scan is the authority.

### Follow-up in the same PR
- Add to `.gitignore`:
  ```
  *.core
  *.core.*
  ```
- Add `scripts/check-blob-hygiene.mjs` and a static-authority test:
  - reject blobs whose first bytes are `7f 45 4c 46` (ELF; core is
    `ET_CORE`), `4d 44 4d 50` (minidump), `fe ed fa ce` / `fe ed fa cf` /
    `ce fa ed fe` / `cf fa ed fe` (Mach-O), `ca fe ba be` (Java class);
  - cap any non-allowlisted blob at a reviewed size (start at 8 MiB);
  - exact allowlist, asserted non-stale.
- Wire it into PR and trusted static scans.

### Acceptance
- `git rev-list --objects --all | grep -E 'qemu_chrome.*\.core'` is empty.
- A seeded ELF/minidump/Mach-O fixture fails the hygiene test.
- Fresh clone is ~179 MB smaller.

## 2. Replace the memory `LIKE` predicate (p.3; row 5, p.4)

### Facts
Current SQL (exact):
- `crates/store/src/ledger.rs:1655` in `memory_facts_page_filtered`:
  `AND kind LIKE '{prefix}%'`
- `crates/store/src/longtask.rs:404` in
  `workspace_memory_facts_page_filtered`: the same predicate.
- `crates/store/src/longtask.rs:506` in `memory_fact_count_v2`:
  `WHERE session_id = ?1 AND kind LIKE 'memory_v2:%'`.

`LIKE` is case-insensitive for ASCII (`MEMORY_V2:x` matches), treats `_` as
a single-character wildcard (`memoryXv2:y` matches), and cannot use the
`(session_id, kind, key)` index. The report measured 49 ms per page at
200,000 churn rows vs 0.02 ms with the range.

### Fix
Replace every predicate with the half-open range (bind the two literals,
do not interpolate):

```sql
AND kind >= 'memory_v2:' AND kind < 'memory_v2;'
```

`:` is `0x3a`, `;` is `0x3b`, so the range is exactly the `memory_v2:`
prefix; `memory_v2;` is the exclusive successor. Apply to both pagers and
the count query. If a generic prefix helper is used, compute the upper bound
by incrementing the last byte and assert `prefix < upper`; never use
`LIKE`.

Add no new index: the table already has
`UNIQUE (session_id, kind, key)` (`crates/store/src/schema.rs:107-115`),
whose `sqlite_autoindex_memory_fact_1` is the exact
`(session_id, kind, key)` index. `workspace_memory_fact` has the analogous
`(workspace_id, kind, key)` primary key. The report's "query-plan gate
naming the `(session_id, kind, key)` index" is satisfied by naming this
existing autoindex; a v40 index would be redundant. (The WIP in the shared
worktree already names it in `ledger_tests.rs`.)

### Tests
- `crates/store/src/query_plan_tests.rs`: `EXPLAIN QUERY PLAN` for both
  pagers and the count must name `sqlite_autoindex_memory_fact_1` (or the
  workspace primary-key index).
- `crates/store/src/ledger_tests.rs` / `longtask_tests.rs`:
  - seed 200,000 internal churn rows;
  - page p50 < 1 ms (report target 0.02 ms);
  - `memory_v2:decision` matches;
  - `MEMORY_V2:x` must not match;
  - `memoryXv2:y` must not match;
  - count equals the number of matching rows, not the wildcard set.
- Run:
  ```
  cargo test -p faktor-store query_plan
  cargo test -p faktor-store memory
  ```

### Acceptance
- `rg "LIKE 'memory_v2" crates/store/src` returns nothing.
- Plan tests name `sqlite_autoindex_memory_fact_1` (or the workspace
  primary-key index); 200k timing passes.

## 3. Close the settlement race and ban lazy `task_id()` reads (p.3; row 1)

### Facts
- The admitted identity is now carried on `RunSettlement::InSession`
  (`crates/orchestrator/src/task_executor/settlement.rs:36-56`), but these
  paths still pass no id:
  - `settle_resolved_verifications` — `task_id: None`
    (`settlement.rs:243-247`);
  - recovery — `task_id: None` (`recovery.rs:570`);
  - the shadow watcher path (`integration.rs:892+`) has no admitted id.
- `settlement_task_id` falls back to a run-row lookup and then the current
  task (`settlement.rs:8-23`); the run row for `tx-session-<id>` contains
  only a policy row, so the fallback wins.
- **WIP (uncommitted):** the shared worktree writes `task_id` into
  `RunPolicyRow`/`DurablePlanRow`, adds `run_policy_task_id`, and refuses
  legacy rows instead of falling back. It does not yet remove the 18 lazy
  reads; audit and finish before claiming the item.
- Lazy reads remain at `settlement.rs:20,283,911`,
  `crates/agent/src/runtime/turn/drive.rs:724,1499`,
  `turn/state.rs:1385,2513`, `compaction.rs:981,1145`,
  `retrieval.rs:948,987`, `verification_attribution.rs:919`,
  `outcome_ledger.rs:2558,2606`, `orchestrator/runtime.rs:977`,
  `runtime_helpers.rs:32` (the report counts 18 in agent and orchestrator
  code).

### Fix
1. Write the durable run linkage row at admission for **every** run path,
   including queued prompt, recovery and shadow watcher.
2. Make `InSession.task_id` a required `TaskId`; remove the `None` variants.
3. Pass the bound id through `settle_run`, `settle_orchestrated`,
   `settle_resolved_verifications`, the watcher and recovery; no settlement
   function may call `handle.task_id()`.
4. Add a static-authority ban:
   - files: `crates/agent/src/runtime/settlement.rs`,
     `turn/drive.rs`, `turn/state.rs`, `compaction.rs`, `retrieval.rs`,
     `verification_attribution.rs`, `outcome_ledger.rs`,
     `crates/orchestrator/src/task_executor/settlement.rs`,
     `recovery.rs`, `integration.rs`, `crates/orchestrator/src/runtime.rs`,
     `runtime_helpers.rs`;
   - pattern: `handle.task_id()` / `.task_id()` used to derive a settlement
     target;
   - allowlist: only the admission-time binding read, exact line, with a
     comment.
5. Add a deterministic interleaving harness:
   - start prompt N+1 at each named await point of turn N's settlement;
   - assert task N+1's durable row is byte-identical (goal, criteria,
     spend, gate);
   - assert task N+1 is not terminal before its first provider call;
   - repeat for queued prompt, recovery and watcher paths;
   - corrupt/remove the linkage row and assert a loud typed failure, not a
     fallback to the current task.
6. Add a crash variant: kill after the linkage write and before settlement;
   the recovered settlement must use the recorded id.

### Acceptance
- The 18 lazy reads are zero in the banned files.
- The interleaving matrix passes for every await point.
- A planted lazy read fails the static scan.
- No task row is closed before its first provider call.

## 4. Schedule the scale probe (p.3; pp.41,45)

### Facts
- Script: `scripts/edition8/scale-probe.mjs`.
- Usage: `node scripts/edition8/scale-probe.mjs --turns 1000` (or `--quick`).
- Output: `target/certification/edition8/scale.raw.json`.
- It needs `target/release/faktor-cli`, `target/release/examples/e8_scale_seed`
  and `e8_append_turn`; it calls cargo only if the seed example is missing.
- No `.woodpecker/**` reference exists today.

### Nightly wiring
Add two steps to `.woodpecker/trusted/nightly.yaml` before
`certificate-nightly`. Build with the same pinned Rust image already used by
`fault-scale`/`longrun`; run with the pinned `node:24` image already used by
certificate steps.

```yaml
  - name: scale-build
    image: rust:1.98.0@sha256:620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959
    environment:
      CARGO_TARGET_DIR: /woodpecker-cache/target
    volumes:
      - faktor-nightly-cargo-registry:/usr/local/cargo/registry
      - faktor-nightly-cargo-git:/usr/local/cargo/git
      - faktor-nightly-target-scale:/woodpecker-cache/target
    commands:
      - cargo build --release -p faktor-cli --example e8_scale_seed --example e8_append_turn
      - mkdir -p target/release/examples
      - cp /woodpecker-cache/target/release/faktor-cli target/release/faktor-cli
      - cp /woodpecker-cache/target/release/examples/e8_scale_seed target/release/examples/
      - cp /woodpecker-cache/target/release/examples/e8_append_turn target/release/examples/

  - name: scale-probe
    image: node:24@sha256:64af3819f9275802414d7cdc38c27e9d82bd564dec4d4da87d008255d36c63b4
    depends_on: [scale-build]
    environment:
      FAKTOR_CLI_BIN: target/release/faktor-cli
    commands:
      - node scripts/edition8/scale-probe.mjs --turns 1000
      - |
        set -eu
        mkdir -p target/certification/lanes
        CMDS="$(cat <<'CMDS'
        node scripts/edition8/scale-probe.mjs --turns 1000
        CMDS
        )"
        printf '%s' "$CMDS" | sh scripts/certification/lane-marker.sh write \
          --lane scale --status passed \
          --artifact target/certification/edition8/scale.raw.json \
          --out target/certification/lanes/scale.json
```

Then make `certificate-nightly` require `--artifact
target/certification/lanes/scale.json` (and the raw JSON) like the existing
fault/longrun markers. Add `scale` to the release certificate's required
lanes.

### Thresholds (already asserted by the script)
- turn curve ≤20 overall, ≤2 steady state, p95 ≤15,000 ms;
- transcript page p95 ≤500 ms, first/last page curve ≤6;
- search ratio 10k→100k ≤25;
- history 2,000 page curve ≤6;
- memory 5,000, child 500, claims 10,000, MCP/skills/hooks 500, 16 streams,
  1,000,000 churn, 1,000 shadowed turns;
- RSS recorded per workload.

### Acceptance
- The nightly cron emits a signed `scale` lane marker.
- A deliberately injected super-linear query (for example the memory `LIKE`
  predicate) fails the lane.
- Release certificate lists `scale` as required.

## 5. Add the 1-micro cap regression (build order step 1, p.46)

The report's build order groups this with the act-now items. Add to
`crates/server/src/api/router_task_tests.rs`:
- a plain prompt with `max_cost_micro = 1` and `max_tokens = 1` makes zero
  provider calls and returns a typed budget refusal;
- a prompt with `max_cost_micro = 0` follows the documented rule
  (`crates/server/src/native/task.rs:731` currently says zero is absent / no
  override); assert that rule explicitly, and ensure the UI shows it;
- remove the cap check in a test build and confirm both tests fail.

Acceptance: the 1-micro test is green at HEAD and fails when the cap check
is removed.
