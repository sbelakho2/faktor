//! `runtime::verification_attribution`: cohesive slice of the agent runtime.

use super::*;

/// Verification-tier escalation (audit 54): a High/Unknown semantic risk
/// forces Strict at the turn's end-of-turn verification/review, while `None`
/// and non-escalating levels keep the configured/default quality exactly.
pub fn verification_quality_for(
    risk: Option<RiskLevel>,
    base: VerificationQuality,
) -> VerificationQuality {
    if semantic_risk_escalates(risk) {
        VerificationQuality::Strict
    } else {
        base
    }
}

/// The provider/model pair of the ACTUAL independent review call: the
/// ROUTED reviewer that produced (or refused) the verdict, recorded from the
/// live call's own outcome. Never the parent session's configured pair — the
/// two differ whenever the review phase routes elsewhere, and the proof
/// basis must name the real reviewer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewModelIdentity {
    pub provider: String,
    pub model: String,
}

/// Extract the ACTUAL review-model identity recorded in one review value
/// (`evidence.structured.review_model`, written by the review path from the
/// live `IndependentReviewOutcome`). Honest `None` when no review-model call
/// was attempted (`attempted != true`) or when the recorded pair is empty
/// (an oversized package refused before routing) — never a fabricated
/// reviewer.
pub fn review_model_identity_of(review: &serde_json::Value) -> Option<ReviewModelIdentity> {
    let model = review
        .get("evidence")?
        .get("structured")?
        .get("review_model")?;
    if model.get("attempted").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let provider = model.get("provider").and_then(|v| v.as_str())?;
    let model_name = model.get("model").and_then(|v| v.as_str())?;
    if provider.is_empty() || model_name.is_empty() {
        return None;
    }
    Some(ReviewModelIdentity {
        provider: provider.to_string(),
        model: model_name.to_string(),
    })
}

/// The typed outcome of ONE integrated-root verification run: the checks the
/// REAL shared [`crate::VerificationService`] executed over the final
/// integration root, their proof rows, and one criterion verdict per
/// acceptance criterion of the parent run.
#[derive(Debug, Clone)]
pub struct IntegratedRootVerification {
    pub status: VerificationStatus,
    pub checks: Vec<CheckExecution>,
    pub criteria: Vec<CriterionVerification>,
    /// The integrated change the checks were derived from.
    pub changed: Vec<String>,
    pub summary: String,
    /// The ROUTED provider/model of the ACTUAL independent review call this
    /// verdict rests on (`None` when no review-model call was attempted —
    /// honest absence, never the parent's configured pair). The orchestrator
    /// folds this pair into the proof basis's reviewer digest.
    pub review_model_identity: Option<ReviewModelIdentity>,
}

/// The outcome of one ATTEMPT-BASED integrated-root verification (audit
/// P0-5/26 production wiring): the EXACT durable verification attempt the
/// verdict belongs to, whether its expensive checks are still open, and the
/// verdict once the attempt is terminal. The caller persists `attempt_op`
/// and, on a later settlement, consumes EXACTLY that attempt — never
/// "whatever the newest attempt happens to be".
#[derive(Debug, Clone)]
pub struct IntegratedRootAttemptOutcome {
    /// The exact durable attempt op this outcome belongs to. When an older
    /// attempt was superseded mid-flight this is the FRESH op, and the older
    /// attempt's open jobs were structurally cancelled (late results can
    /// never resolve this one — the store refuses).
    pub attempt_op: u64,
    /// True when at least one required durable job is still open: the run is
    /// not certified, must not land, and waits for the executor (or a later
    /// settlement) to settle this exact attempt.
    pub pending: bool,
    /// The verdict once the attempt is terminal; `None` while pending.
    pub verification: Option<IntegratedRootVerification>,
}

/// Completion classification at a genuine turn end (audits 4/6/7): the
/// durable gate between "the turn ended" and "the change is verified
/// complete". Only [`CompletionGate::VerifiedComplete`] may present the
/// task's work as complete — and only when every required check the project
/// type derived for this turn's changes ran AND passed. The gate is
/// computed at the SAME sites that run the verifier/review and assign
/// `TurnOutcome.verification/acceptance/review`, and is stored as durable
/// memory rows (`task_state`/`state`, `verification`/`last`) before the
/// `TurnCompleted` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionGate {
    /// Every required check ran and passed; the change is durably verified.
    VerifiedComplete,
    /// No objective verification mechanism confirmed the change (no verifier
    /// wired, workspace unresolvable, or nothing derivable): the turn is
    /// NEVER silently complete.
    Unverified,
    /// Required checks could not gate success: a check the change required
    /// could not run (execution infra delivered no verdict), or the review
    /// blocked (skeptical-review gate), or the durable spend already exceeds
    /// the task budget, or — in Strict quality — the durable criteria rows
    /// disagree. Every reason carries a machine [`ReasonCode`] + human
    /// detail (audit 94).
    BlockedVerification { reasons: Vec<OutcomeReason> },
    /// A required check RAN and failed: the change is not complete.
    /// `outcome.acceptance` is Fail, but the session stays usable — the
    /// next turn may fix and re-verify.
    FailedVerification { reasons: Vec<OutcomeReason> },
    /// Required checks of this attempt run as DURABLE background
    /// verification jobs (audit P0-5/26): the checks could not run inline
    /// within the turn, so the attempt is persisted and the task waits at
    /// Verifying until every job of the attempt settles at a later genuine
    /// end. The gate is NEVER a completion and never a blocker — it is the
    /// honest mid-flight state of a real verification attempt.
    VerificationPending,
}

impl CompletionGate {
    /// The durable `task_state` FACT value for this gate (audits 4/6/7): the
    /// compaction-proof memory row keeps recording the GATE (VerifiedComplete
    /// stays VerifiedComplete; Unverified is explicitly NeedsVerification
    /// (never Pending/complete); Blocked stays Blocked; a failed verification
    /// records Failed; a pending background attempt records Verifying). The
    /// typed task ROW is no longer patched to these values: audit P0-7's
    /// machine drives it through legal transitions (`sync_task_row` +
    /// `apply_gate_to_task_row`, whose comment table maps every gate to its
    /// machine writes — e.g. a retryable failed gate lands the row at
    /// NeedsVerification while the fact records Failed).
    pub fn task_state(&self) -> TaskState {
        match self {
            CompletionGate::VerifiedComplete => TaskState::VerifiedComplete,
            CompletionGate::Unverified => TaskState::NeedsVerification,
            CompletionGate::BlockedVerification { .. } => TaskState::Blocked,
            CompletionGate::FailedVerification { .. } => TaskState::Failed,
            CompletionGate::VerificationPending => TaskState::Verifying,
        }
    }
}

/// End-of-turn verification/review strictness (audit 92's runtime meaning).
/// The completion-proof spec (`docs/specs/completion_proof.md`) defines no
/// named quality tiers; its rules bind MUTATING coding tasks
/// (Completed-without-a-record is illegal, weakening/deletion fails review,
/// repo state must back "done" claims). This runtime therefore applies the
/// strict bar wherever a quality choice is NOT explicit: **mutating turns
/// (files changed) default to [`VerificationQuality::Strict`]**, while
/// non-mutating turns (no completion claim at stake) default to Normal.
/// `Normal` is byte-for-byte today's behavior (only a review verdict
/// `"block"` gates; no per-turn criteria-row verification). `Strict`
/// raises the bar exactly where the spec's rules bite:
///  - review runs under the same conditions, but ANY non-clean review — a
///    `"block"` verdict, a non-`"pass"` verdict shape (e.g. a hostile
///    `"weakened"` label), or advisory suspects on the changed code — gates
///    `BlockedVerification` instead of only `"block"`;
///  - the durable criteria fact (`criteria`/`0`, wave-9 row) is verified
///    against the typed task row's acceptance criteria at every genuine
///    turn end: a disagreement (crash residue or a hostile write) refuses
///    the completion claim with a machine `criteria_inconsistent` reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerificationQuality {
    /// Today's review/verification behavior, unchanged.
    #[default]
    Normal,
    /// The strict review bar for mutating tasks (the default when no
    /// explicit quality is set and the turn changed files).
    Strict,
}

/// The end-of-turn verdict assembled at the two genuine turn ends: the raw
/// per-check results, the required-only acceptance, the advisory review and
/// the completion gate (see [`CompletionGate`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnEndVerdict {
    pub(crate) verification: Vec<(String, bool)>,
    pub(crate) acceptance: Option<faktor_verify::Acceptance>,
    pub(crate) review: Option<serde_json::Value>,
    pub(crate) completion: Option<CompletionGate>,
    /// The acceptance-criteria entries (goal + derived required checks)
    /// frozen at this gate; seeded into the durable task row.
    pub(crate) criteria: Option<Vec<String>>,
    /// The durable-proof payload of the verification attempt (audit P0-8):
    /// the executed checks, criterion verdicts and changed-file observations
    /// a per-attempt [`faktor_session::VerificationRecord`] is built from.
    /// `Some` only when the required checks produced a verdict (records are
    /// per-attempt evidence; an attempt with no verdict has nothing to
    /// record).
    pub(crate) proof: Option<VerificationProof>,
}

/// One genuine-end verification attempt's durable-proof payload (audit
/// P0-8): everything a per-attempt [`faktor_session::VerificationRecord`]
/// carries that the runtime observed at the verification site. Built ONLY
/// where the workspace handle is open (the same site that runs the checks),
/// so the record's evidence is content-addressed against the same repo state
/// the checks saw.
#[derive(Debug, Clone, Default)]
pub(crate) struct VerificationProof {
    /// One execution row per required check that RAN (pass or fail);
    /// checks the infra could not run carry no execution row.
    pub(crate) checks: Vec<CheckExecution>,
    /// One verdict per acceptance-criteria entry (goal + required checks) —
    /// the SAME texts that seed the typed task row, so a later completion's
    /// coverage check compares identical keys.
    pub(crate) criteria: Vec<CriterionVerification>,
    /// Bounded content-addressed observations of the changed files (whole
    /// file, streamed; unreadable files are skipped like review heads).
    pub(crate) changed_files: Vec<FileStateEvidence>,
    /// The advisory review verdict when it fits the record's opaque bound.
    pub(crate) review: Option<serde_json::Value>,
}

/// One changed file's fetched evidence: the existence/hash state (rows or
/// disk fallback) plus bounded before/after bytes when both sides are
/// available for an honest line diff.
pub(crate) struct ReviewFetchedFile {
    pub(crate) path: String,
    /// Checkpoint existence/hash state when a checkpoint row exists;
    /// disk-derived otherwise (before unknown → both sides read existing).
    pub(crate) change: faktor_verify::review::FileChange,
    pub(crate) before_bytes: Option<Vec<u8>>,
    pub(crate) after_bytes: Option<Vec<u8>>,
    /// Rows were the source of this file's state (vs. a disk-only fallback).
    pub(crate) from_rows: bool,
}

/// Fetch the per-path before/after evidence for the review decision.
/// Primary source: the session's checkpoint rows + CAS blobs (per-path
/// earliest before → latest after, so nothing the turn wrote can hide below
/// the base). Files with no checkpoint row fall back to a bounded current
/// read (status Modified when readable, Deleted when gone — with no base,
/// "added" cannot be proven and no hunks are fabricated). A per-side
/// content beyond [`REVIEW_SIDE_BOUND`] refuses the WHOLE review with an
/// oversize reason — never a partial package.
pub(crate) fn review_fetch_changed_files(
    deps: &AgentDeps,
    handle: &faktor_session::SessionHandle,
    ws: &faktor_fs::WorkspaceHandle,
    changed: &[String],
) -> (Vec<ReviewFetchedFile>, Option<String>) {
    // P2-VERIFY: a checkpoint-store READ FAILURE refuses the review (the
    // diff base would silently degrade to "before unknown"); only an absent
    // store is the legitimate empty case.
    let rows = match deps.snapshots.as_ref() {
        None => Vec::new(),
        Some(snapshots) => match snapshots.checkpoints(handle.id()) {
            Ok(rows) => rows,
            Err(e) => {
                return (
                    Vec::new(),
                    Some(format!(
                        "checkpoint rows of session {} are unreadable ({e}); the review diff base cannot be established",
                        handle.id()
                    )),
                )
            }
        },
    };
    let cas = deps.cas.as_ref();
    let mut out: Vec<ReviewFetchedFile> = Vec::new();
    for path in changed
        .iter()
        .take(faktor_verify::review::REVIEW_MAX_CHANGED_FILES)
    {
        let mut rows_p: Vec<&faktor_store::CheckpointRow> =
            rows.iter().filter(|r| &r.path == path).collect();
        rows_p.sort_by_key(|r| r.sequence);
        if let (Some(first), Some(last)) = (rows_p.first(), rows_p.last()) {
            if let Some(cas) = cas {
                // A missing BEFORE side is a real creation (the state, not
                // an unknown): the diff base is the empty file — the whole
                // new content is the change and must ride the hunks.
                let before = if first.before_exists {
                    match file_hash_from_row(&first.before_hash) {
                        Some(h) => match cas.get_bounded(h, REVIEW_SIDE_BOUND) {
                            Ok(Some(bytes)) => Some(bytes),
                            _ => {
                                return (
                                    out,
                                    Some(format!(
                                        "{path}: before content exceeds the {REVIEW_SIDE_BOUND} byte review bound"
                                    )),
                                )
                            }
                        },
                        None => None,
                    }
                } else {
                    Some(Vec::new())
                };
                let after = if last.after_exists {
                    let blob = last
                        .after_cas_hash
                        .as_deref()
                        .and_then(file_hash_from_row)
                        .and_then(|h| cas.get_bounded(h, REVIEW_SIDE_BOUND).ok().flatten());
                    match blob {
                        Some(bytes) => Some(bytes),
                        None => {
                            // Pre-v3 row (no after blob) or a blob read
                            // problem: fall back to the CURRENT workspace
                            // content (the row's after state is what the
                            // tool wrote; the disk is the reviewer's truth).
                            match ws_read_bounded(ws, path) {
                                Ok(Some(bytes)) => Some(bytes),
                                Ok(None) => {
                                    return (
                                        out,
                                        Some(format!(
                                            "{path}: after content exceeds the {REVIEW_SIDE_BOUND} byte review bound"
                                        )),
                                    )
                                }
                                Err(_) => None,
                            }
                        }
                    }
                } else {
                    None
                };
                out.push(ReviewFetchedFile {
                    path: path.clone(),
                    change: faktor_verify::review::FileChange {
                        path: path.clone(),
                        before_exists: first.before_exists,
                        before_hash_hex: first.before_exists.then(|| first.before_hash.clone()),
                        after_exists: last.after_exists,
                        after_hash_hex: last.after_exists.then(|| last.after_hash.clone()),
                    },
                    before_bytes: before,
                    after_bytes: after,
                    from_rows: true,
                });
                continue;
            }
        }
        // Disk-only fallback: no rows (snapshots not wired or the write was
        // not checkpointed). Bounded current content; a missing file reads
        // Deleted. Before content is unknowable — no hunks are fabricated.
        let change = match ws_read_bounded(ws, path) {
            Ok(Some(bytes)) => {
                let full_hash = ws
                    .read(std::path::Path::new(path), REVIEW_SIDE_BOUND)
                    .ok()
                    .and_then(|d| d.full_hash());
                out.push(ReviewFetchedFile {
                    path: path.clone(),
                    change: faktor_verify::review::FileChange {
                        path: path.clone(),
                        before_exists: true,
                        before_hash_hex: None,
                        after_exists: true,
                        after_hash_hex: full_hash.map(|h| h.to_hex()),
                    },
                    before_bytes: None,
                    after_bytes: Some(bytes),
                    from_rows: false,
                });
                continue;
            }
            Ok(None) => {
                return (
                    out,
                    Some(format!(
                        "{path}: content exceeds the {REVIEW_SIDE_BOUND} byte review bound"
                    )),
                )
            }
            // Unreadable = deleted (or unresolved hostile path).
            Err(_) => faktor_verify::review::FileChange {
                path: path.clone(),
                before_exists: true,
                before_hash_hex: None,
                after_exists: false,
                after_hash_hex: None,
            },
        };
        out.push(ReviewFetchedFile {
            path: path.clone(),
            change,
            before_bytes: None,
            after_bytes: None,
            from_rows: false,
        });
    }
    if changed.len() > faktor_verify::review::REVIEW_MAX_CHANGED_FILES {
        return (
            out,
            Some(format!(
                "{} changed files exceed the {} file review bound",
                changed.len(),
                faktor_verify::review::REVIEW_MAX_CHANGED_FILES
            )),
        );
    }
    (out, None)
}

/// Added-line signal scan over one file's hunks (structured counterpart of
/// the legacy 400-char head scan — this one sees EVERY added line, bounded
/// only by [`REVIEW_ADDED_SCAN_CHARS`]).
pub(crate) struct ReviewAddedSignals {
    pub(crate) scan_chars: usize,
    pub(crate) has_added_lines: bool,
    pub(crate) has_removed_lines: bool,
    pub(crate) contains_todo: bool,
    /// A literal stub marker line ("...", "// todo: implement") among the
    /// added lines. Placeholder BODIES (short files) stay with the legacy
    /// whole-file head scan: a partial edit adding few short lines into a
    /// real file is NOT a stub, and only the head scan can see file size.
    pub(crate) stub_marker: bool,
    pub(crate) test_markers: bool,
    pub(crate) assertions_added: bool,
    pub(crate) assertions_removed: usize,
}

pub(crate) fn review_added_signals(hunks: &[&faktor_verify::review::Hunk]) -> ReviewAddedSignals {
    let mut added = String::new();
    let mut sig = ReviewAddedSignals {
        scan_chars: 0,
        has_added_lines: false,
        has_removed_lines: false,
        contains_todo: false,
        stub_marker: false,
        test_markers: false,
        assertions_added: false,
        assertions_removed: 0,
    };
    for hunk in hunks {
        for line in &hunk.lines {
            match line.kind {
                faktor_verify::review::DiffKind::Added => {
                    sig.has_added_lines = true;
                    let trimmed = line.text.trim().to_lowercase();
                    if REVIEW_STUB_LINES.contains(&trimmed.as_str()) {
                        sig.stub_marker = true;
                    }
                    if sig.scan_chars < REVIEW_ADDED_SCAN_CHARS {
                        let room = REVIEW_ADDED_SCAN_CHARS - sig.scan_chars;
                        let take: String = line.text.chars().take(room).collect();
                        sig.scan_chars += take.chars().count();
                        added.push_str(&take);
                    }
                }
                faktor_verify::review::DiffKind::Removed => {
                    sig.has_removed_lines = true;
                    if line_assertion_like(&line.text) {
                        sig.assertions_removed = sig.assertions_removed.saturating_add(1);
                    }
                }
                faktor_verify::review::DiffKind::Context => {}
            }
        }
    }
    if sig.has_added_lines {
        let lower = added.to_lowercase();
        sig.contains_todo = REVIEW_TODO_TOKENS.iter().any(|t| lower.contains(t));
        sig.test_markers = REVIEW_TEST_MARKERS.iter().any(|m| added.contains(m));
        sig.assertions_added = line_assertion_like(&added);
    }
    sig
}

/// The structured review evidence: statuses + hunks + inventory + risk +
/// the rendered package. Local structured findings (beyond the legacy head
/// scan) ride `blocking`/`suspects`; `oversize` carries the refusal text
/// when the change is too large for an honest package (never truncated).
pub(crate) struct StructuredReviewEvidence {
    pub(crate) value: serde_json::Value,
    pub(crate) blocking: Vec<String>,
    pub(crate) suspects: Vec<String>,
    pub(crate) package_json: Option<String>,
    pub(crate) oversize: Option<String>,
    pub(crate) risk: faktor_verify::review::RiskAssessment,
}

/// Build the structured evidence + local findings + package for one turn's
/// change set (pure assembly over the fetched files; I/O already done).
pub(crate) fn structured_review_evidence(
    deps: &AgentDeps,
    handle: &faktor_session::SessionHandle,
    ws: &faktor_fs::WorkspaceHandle,
    changed: &[String],
    criteria: &[String],
    checks: &[faktor_verify::Check],
) -> StructuredReviewEvidence {
    let (fetched, fetch_oversize) = review_fetch_changed_files(deps, handle, ws, changed);
    let changes: Vec<faktor_verify::review::FileChange> =
        fetched.iter().map(|f| f.change.clone()).collect();
    let mut statuses = faktor_verify::review::classify_file_statuses(&changes);
    for (status, f) in statuses.iter_mut().zip(fetched.iter()) {
        if let Some(b) = &f.before_bytes {
            status.bytes_before = Some(b.len() as u64);
        }
        if let Some(b) = &f.after_bytes {
            status.bytes_after = Some(b.len() as u64);
        }
    }
    // Line hunks over every pair with both sides available. A coarse/side
    // refusal poisons the whole review (never a partial package).
    let mut hunks: Vec<faktor_verify::review::Hunk> = Vec::new();
    let mut hunks_oversize: Option<String> = fetch_oversize;
    if hunks_oversize.is_none() {
        for f in &fetched {
            if let (Some(before), Some(after)) = (&f.before_bytes, &f.after_bytes) {
                match review_hunks_for(&f.path, before, after) {
                    Ok(h) => hunks.extend(h),
                    Err(e) => {
                        hunks_oversize = Some(e);
                        break;
                    }
                }
            }
        }
    }
    let inventory = faktor_verify::review::compute_test_inventory(&statuses, &hunks);
    let risk = faktor_verify::review::assess_change_risk(changed, &inventory.removed_tests);

    // ---- local structured findings (in addition to the legacy head scan)
    let mut blocking: Vec<String> = Vec::new();
    let mut suspects: Vec<String> = Vec::new();
    for path in &inventory.removed_tests {
        blocking.push(format!("deleted test file: {path}"));
    }
    if hunks_oversize.is_none() {
        let by_path: std::collections::HashMap<&str, Vec<&faktor_verify::review::Hunk>> = hunks
            .iter()
            .fold(std::collections::HashMap::new(), |mut m, h| {
                m.entry(h.path.as_str()).or_default().push(h);
                m
            });
        let mut paths: Vec<&str> = by_path.keys().copied().collect();
        paths.sort_unstable();
        for path in paths {
            let file_hunks: Vec<&faktor_verify::review::Hunk> = by_path[path].clone();
            let sig = review_added_signals(&file_hunks);
            if !sig.has_added_lines && !sig.has_removed_lines {
                continue;
            }
            let path_test = review_path_is_test(path) || sig.test_markers;
            let weakened = sig.test_markers && !sig.assertions_added;
            if weakened && sig.has_added_lines {
                blocking.push(format!("weakened test file without assertions: {path}"));
            } else if sig.contains_todo && sig.stub_marker {
                blocking.push(format!("placeholder/TODO in changed code: {path}"));
            } else if sig.contains_todo {
                suspects.push(format!("contains TODO in changed file: {path}"));
            } else if sig.stub_marker && !weakened {
                suspects.push(format!("placeholder body in changed file: {path}"));
            }
            if path_test
                && sig.has_removed_lines
                && sig.assertions_removed > 0
                && !sig.assertions_added
                && sig.has_added_lines
            {
                blocking.push(format!(
                    "test assertions removed without replacement: {path}"
                ));
            }
        }
    }

    // ---- package build + render (hard bounds; oversize is a refusal)
    let mut package_json = None;
    let mut oversize = hunks_oversize;
    if oversize.is_none() {
        match faktor_verify::review::build_package(
            criteria,
            statuses.clone(),
            hunks.clone(),
            review_check_rows(checks),
        ) {
            Ok(package) => match faktor_verify::review::render_package(&package) {
                Ok(json) => package_json = Some(json),
                Err(e) => oversize = Some(e.to_string()),
            },
            Err(e) => oversize = Some(e.to_string()),
        }
    }
    let source = if fetched.iter().any(|f| f.from_rows) {
        "checkpoint_cas"
    } else {
        "workspace_heads"
    };
    let mut value = serde_json::json!({
        "source": source,
        "package_bytes": package_json.as_ref().map(|j| j.len()),
    });
    if let Some(o) = &oversize {
        value["oversize"] = serde_json::json!(o);
    }
    let total_files = statuses.len();
    let file_rows: Vec<serde_json::Value> = statuses
        .iter()
        .take(REVIEW_EVIDENCE_MAX_FILES)
        .map(|s| {
            serde_json::json!({
                "path": truncate(&s.path, 300),
                "status": s.status.as_str(),
                "renamed_from": s.renamed_from.as_deref().map(truncate_300),
                "renamed_to": s.renamed_to.as_deref().map(truncate_300),
                "bytes_before": s.bytes_before,
                "bytes_after": s.bytes_after,
            })
        })
        .collect();
    value["files"] = serde_json::json!(file_rows);
    value["total_files"] = serde_json::json!(total_files);
    // Per-file hunk summaries (line/content counts; the full lines ride the
    // transient package only — the review JSON stays record-bounded).
    let by_path: std::collections::HashMap<&str, Vec<&faktor_verify::review::Hunk>> = hunks
        .iter()
        .fold(std::collections::HashMap::new(), |mut m, h| {
            m.entry(h.path.as_str()).or_default().push(h);
            m
        });
    let mut hunk_rows: Vec<serde_json::Value> = Vec::new();
    for (path, hs) in by_path.iter() {
        let mut added_lines = 0usize;
        let mut removed_lines = 0usize;
        let mut added_chars = 0usize;
        for h in hs.iter() {
            for l in &h.lines {
                match l.kind {
                    faktor_verify::review::DiffKind::Added => {
                        added_lines += 1;
                        added_chars += l.text.len();
                    }
                    faktor_verify::review::DiffKind::Removed => removed_lines += 1,
                    faktor_verify::review::DiffKind::Context => {}
                }
            }
        }
        hunk_rows.push(serde_json::json!({
            "path": truncate(path, 300),
            "hunks": hs.len(),
            "added_lines": added_lines,
            "removed_lines": removed_lines,
            "added_chars": added_chars,
        }));
        if hunk_rows.len() >= REVIEW_EVIDENCE_MAX_HUNK_ROWS {
            break;
        }
    }
    value["hunks"] = serde_json::json!(hunk_rows);
    let entry = |v: &[String]| -> Vec<String> {
        v.iter()
            .take(REVIEW_EVIDENCE_MAX_PATH_ENTRIES)
            .map(|p| truncate(p, 300))
            .collect()
    };
    value["test_files_changed"] =
        serde_json::json!(entry(&inventory_list(
            &statuses,
            |s| faktor_verify::review::path_is_test(&s.path)
                && !matches!(
                    s.status,
                    faktor_verify::review::FileChangeStatus::Deleted
                        | faktor_verify::review::FileChangeStatus::Renamed
                )
        )));
    value["deleted_tests"] = serde_json::json!(entry(&inventory.removed_tests));
    value["ci_build_files_changed"] = serde_json::json!(entry(&inventory.ci_workflow_changes));
    value["criteria"] = serde_json::json!(criteria
        .iter()
        .take(REVIEW_EVIDENCE_MAX_CRITERIA)
        .map(|c| truncate(c, 300))
        .collect::<Vec<_>>());
    value["check_results"] = serde_json::to_value(review_check_rows(checks)).unwrap_or_default();
    value["inventory"] = serde_json::to_value(&inventory).unwrap_or_default();
    value["risk"] = serde_json::to_value(&risk).unwrap_or_default();
    StructuredReviewEvidence {
        value,
        blocking,
        suspects,
        package_json,
        oversize,
        risk,
    }
}

/// The outcome of one independent review-model call (P0-13).
pub(crate) struct IndependentReviewOutcome {
    /// Typed model verdict when the call completed and parsed.
    pub(crate) verdict: Option<faktor_verify::review::ReviewVerdict>,
    /// Fail-closed refusal: routing/provider/budget/output refused the
    /// review — the risky change cannot clear the gate unreviewed. There is
    /// no degradation: a routing refusal is a refusal, never a silent
    /// local-signal verdict.
    pub(crate) refused: Option<String>,
    pub(crate) provider: String,
    pub(crate) model: String,
    /// The accepted reviewer output (audit item 3): the raw verdict text
    /// under its explicit [`crate::OutputTrust::VerificationOpinion`] class
    /// and the physical call's durable id. `None` on every refusal — no
    /// evidence value exists when no verdict was authored.
    pub(crate) output: Option<ModelOutput>,
}

impl IndependentReviewOutcome {
    /// A completed review with its typed verdict AND the admitted reviewer
    /// output that authored it. The output MUST carry
    /// [`crate::OutputTrust::VerificationOpinion`]; anything else is a
    /// typed [`TrustRefusal`] (fail closed: the caller refuses the review).
    pub(crate) fn with_verdict(
        provider: &str,
        model: &str,
        verdict: faktor_verify::review::ReviewVerdict,
        output: ModelOutput,
    ) -> Result<Self, TrustRefusal> {
        output.verification_evidence()?;
        Ok(Self {
            verdict: Some(verdict),
            refused: None,
            provider: provider.into(),
            model: model.into(),
            output: Some(output),
        })
    }

    pub(crate) fn refused(provider: &str, model: &str, reason: impl Into<String>) -> Self {
        Self {
            verdict: None,
            refused: Some(reason.into()),
            provider: provider.into(),
            model: model.into(),
            output: None,
        }
    }

    /// The admitted reviewer output, when this outcome carries one.
    pub(crate) fn output(&self) -> Option<&ModelOutput> {
        self.output.as_ref()
    }
}

/// Verification evidence authored by a model call (audit item 3): the
/// review value a [`crate::OutputTrust::VerificationOpinion`] output
/// produced. The only two admittance paths are the LIVE reviewer output
/// ([`ReviewEvidence::from_review_output`]) and a DURABLE review row that
/// carries the same provenance tag ([`ReviewEvidence::from_durable`]) — a
/// context-compression summary or an ephemeral output can take neither, so
/// it can never author criterion/review evidence. Every consumer of review
/// evidence (criterion verdicts, proof rows, reviewer ports) takes THIS
/// type, never a raw JSON value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewEvidence {
    value: serde_json::Value,
}

impl ReviewEvidence {
    /// The provenance tag key recorded in the review value at authoring
    /// time (`output_trust`: the authoring class's
    /// [`crate::OutputTrust::provenance_tag`]).
    pub(crate) const OUTPUT_TRUST_KEY: &'static str = "output_trust";

    /// The provenance tag of deterministic LOCAL review evidence (the
    /// bounded head scan + structured diff package, no model-authored
    /// verdict).
    pub(crate) const DETERMINISTIC_LOCAL_TAG: &'static str = "deterministic_local";

    /// Admit deterministic LOCAL review evidence: a value that carries NO
    /// model-authored reviewer verdict. A value that CLAIMS a reviewer
    /// attempt (`evidence.structured.review_model.attempted == true`) is a
    /// typed refusal — that claim requires the admitted reviewer output
    /// through [`ReviewEvidence::from_review_output`].
    pub(crate) fn from_deterministic_local(
        mut value: serde_json::Value,
    ) -> Result<Self, TrustRefusal> {
        if review_claims_reviewer_attempt(&value) {
            return Err(TrustRefusal {
                trust: OutputTrust::Ephemeral,
                required: "the admitted reviewer output that authored this review \
                           (a claimed reviewer attempt cannot ride local evidence)",
            });
        }
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                Self::OUTPUT_TRUST_KEY.into(),
                serde_json::json!(Self::DETERMINISTIC_LOCAL_TAG),
            );
        }
        Ok(Self { value })
    }

    /// Admit a LIVE reviewer output: enforces the verification-opinion trust
    /// class, then stamps the provenance tag + call id onto the value so any
    /// durable round-trip re-admits through [`ReviewEvidence::from_durable`].
    pub(crate) fn from_review_output(
        output: &ModelOutput,
        mut value: serde_json::Value,
    ) -> Result<Self, TrustRefusal> {
        output.verification_evidence()?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                Self::OUTPUT_TRUST_KEY.into(),
                serde_json::json!(output.trust.provenance_tag()),
            );
            obj.insert("output_call_id".into(), serde_json::json!(output.call_id));
        }
        Ok(Self { value })
    }

    /// Admit a durable review row: it must carry the provenance tag written
    /// by [`ReviewEvidence::from_review_output`], and the tag must name an
    /// evidence-authoring class. A row without the tag (legacy/foreign) or
    /// with a non-authoring tag is a typed refusal — the review contributes
    /// NOTHING rather than being trusted.
    pub(crate) fn from_durable(value: serde_json::Value) -> Result<Self, TrustRefusal> {
        let tag = value.get(Self::OUTPUT_TRUST_KEY).and_then(|v| v.as_str());
        if tag == Some(Self::DETERMINISTIC_LOCAL_TAG) {
            // Same claim check as the live local path: a durable row cannot
            // claim a reviewer attempt it never proved with an output.
            if review_claims_reviewer_attempt(&value) {
                return Err(TrustRefusal {
                    trust: OutputTrust::Ephemeral,
                    required: "the admitted reviewer output that authored this review",
                });
            }
            return Ok(Self { value });
        }
        let trust = tag.and_then(OutputTrust::from_provenance_tag);
        match trust {
            Some(trust) if trust.may_author_verification_evidence() => Ok(Self { value }),
            _ => Err(TrustRefusal {
                trust: trust.unwrap_or(OutputTrust::Ephemeral),
                required: "a durable review row authored by a verification opinion \
                           (output_trust = verification_opinion)",
            }),
        }
    }

    pub(crate) fn as_value(&self) -> &serde_json::Value {
        &self.value
    }

    pub(crate) fn into_value(self) -> serde_json::Value {
        self.value
    }
}

/// Whether a review value claims a reviewer-model attempt
/// (`evidence.structured.review_model.attempted == true`). A value making
/// that claim may only be admitted alongside its reviewer output.
fn review_claims_reviewer_attempt(value: &serde_json::Value) -> bool {
    value
        .get("evidence")
        .and_then(|e| e.get("structured"))
        .and_then(|s| s.get("review_model"))
        .and_then(|m| m.get("attempted"))
        .and_then(|a| a.as_bool())
        == Some(true)
}

/// Admit a review value that crossed a durable/raw boundary: the
/// provenance tag must name an evidence-authoring class (audit item 3).
/// Untrusted or untagged values yield `None` — the caller then treats the
/// value as findings only (reviewer-dependent criteria stay unresolved,
/// fail closed) and NEVER as evidence.
pub(crate) fn admit_review_evidence(review: Option<&serde_json::Value>) -> Option<ReviewEvidence> {
    review.and_then(|value| match ReviewEvidence::from_durable(value.clone()) {
        Ok(evidence) => Some(evidence),
        Err(refusal) => {
            tracing::warn!("review evidence refused by the output-trust gate: {refusal}");
            None
        }
    })
}

/// The real separate review-model call (P0-13): route the Review phase
/// through the SAME routing policy as every paid call, resolve the provider,
/// stream ONE request that carries ONLY the package + criteria + the review
/// contract (no transcript, no implementation context), and parse the typed
/// verdict. Every failure is a fail-closed refusal (there is no
/// RouterUnavailable degradation: a risky change never clears the gate
/// unreviewed). The call is bounded by [`REVIEW_MODEL_CALL_TIMEOUT`] and
/// its wire request inherits a child of the turn's cancellation token.
pub(crate) async fn run_independent_review_call(
    deps: &AgentDeps,
    handle: &faktor_session::SessionHandle,
    package_json: &str,
    criteria: &[String],
    semantic_risk: Option<RiskLevel>,
    cancel: &CancellationToken,
) -> IndependentReviewOutcome {
    let session = handle.id();
    // Routing consult (phase Review) — the decision fixes provider/model.
    // A session whose durable task identity is unresolvable falls back to
    // the documented standalone default (1); TaskId::new(0) is never legal.
    let task_id = handle.task_id().unwrap_or_else(|_| TaskId::new(1));
    let mut provider_id = handle.provider().unwrap_or_default();
    let mut model = handle.model().unwrap_or_default();
    // The review is a PAID call: its budget read obeys the same fail-safe
    // policy as the drive's. A hard-cap (or no-cap-evidence) read failure
    // refuses the review typedly — no provider call ever leaves; only a
    // read that PROVED the task explicitly uncapped proceeds, with
    // accounting-unavailable telemetry.
    let view = match deps.budgets.session_budget_view(session, task_id) {
        Ok(view) => Some(view),
        Err(read_err) if read_err.read_failure_is_explicitly_uncapped() => {
            tracing::warn!(
                session = %session,
                "budget accounting unavailable for an explicitly uncapped review call: \
                 {read_err}; the review proceeds WITHOUT durable budget accounting"
            );
            None
        }
        Err(read_err) => {
            return IndependentReviewOutcome::refused(
                &provider_id,
                &model,
                format!("budget accounting unavailable: {read_err}"),
            );
        }
    };
    let remaining = match &view {
        Some(v) if v.max_cost_micro.is_some() => v.free().min(i64::MAX as u64),
        _ => 0,
    };
    let context_estimate =
        ((package_json.len() + criteria.iter().map(|c| c.len()).sum::<usize>()) / 4) as u64;
    // The route sees the REAL review dimensions: the bounded package +
    // criteria input estimate (with the fixed prompt envelope) and the
    // small typed-verdict output cap. Review-strength escalation (audits
    // 54/118/119): a High/Unknown semantic risk raises the review phase's
    // quality FLOOR through the existing routing quality requirement, so a
    // risky patch cannot be reviewed by a merely-cheap low-quality model.
    let mut intent = crate::ModelCallIntent::review();
    if semantic_risk_escalates(semantic_risk) {
        intent.quality = crate::QualityRequirement::Hard {
            minimum: REVIEW_ESCALATED_QUALITY_FLOOR,
        };
    }
    let req = intent.route_request(
        context_estimate.saturating_add(1024).min(32_768),
        2048,
        remaining,
    );
    // P0-1: when the router priced the review call, its price capture rides
    // the reservation (None on the unpriced session-defaults passthrough:
    // settlement then records an honest Unknown spend instead of a
    // fabricated local price).
    let mut pricing_snapshot: Option<PricingSnapshot> = None;
    match deps.routing.route(&req) {
        Ok(d) if d.provider.is_empty() && d.model.is_empty() => {}
        Ok(d) => {
            pricing_snapshot = d.pricing_snapshot;
            provider_id = d.provider.clone();
            model = d.model.clone();
        }
        Err(f) => {
            // Fail closed: every routing failure refuses the review (no
            // RouterUnavailable degradation to local signals exists
            // anymore) — a risky change never clears the gate unreviewed.
            return IndependentReviewOutcome::refused(
                &provider_id,
                &model,
                format!("routing refused the review call: {f:?}"),
            );
        }
    }
    let Some(provider) = deps.providers.get(&provider_id) else {
        return IndependentReviewOutcome::refused(
            &provider_id,
            &model,
            format!("review provider {provider_id:?} is not registered"),
        );
    };
    // Bounded prompt: criteria + package + contract — nothing else.
    let mut prompt = String::new();
    prompt.push_str("Acceptance criteria to review against:\n");
    for c in criteria
        .iter()
        .take(faktor_verify::review::REVIEW_MAX_CRITERIA_ENTRIES)
    {
        prompt.push_str("- ");
        prompt.push_str(&truncate(c, 300));
        prompt.push('\n');
    }
    prompt.push_str("\nStructured diff package (JSON):\n");
    prompt.push_str(package_json);
    prompt.push_str(
        "\n\nNow respond with ONLY the JSON verdict object described in your instructions.",
    );
    let op_id = match deps.session.try_next_op_id() {
        Ok(id) => id,
        Err(e) => {
            return IndependentReviewOutcome::refused(
                &provider_id,
                &model,
                format!("op-id allocation failed for the review call: {e}"),
            )
        }
    };
    // Attempt identity (attempt-accounting audit): the review call is ONE
    // logical op with ONE physical attempt — the attempt rides a fresh
    // attempt op id so its reservation and its attempt-keyed provider-call
    // rows are joinable exactly like every other paid call (a crashed
    // review reservation reconciles from its OWN completed row).
    let attempt_op_id = match deps.session.try_next_op_id() {
        Ok(id) => id,
        Err(e) => {
            return IndependentReviewOutcome::refused(
                &provider_id,
                &model,
                format!("op-id allocation failed for the review attempt: {e}"),
            )
        }
    };
    let attempt_identity = match ModelCallAttempt::new(op_id, attempt_op_id, 0) {
        Some(a) => a,
        None => {
            return IndependentReviewOutcome::refused(
                &provider_id,
                &model,
                "the review attempt op id collided with the review op id",
            )
        }
    };
    let predicted = (prompt.len() as u64 / 3)
        .saturating_add(2048)
        .saturating_add(256);
    let reservation = match deps
        .budgets
        .reserve_attempt(
            session,
            task_id,
            attempt_identity,
            predicted,
            pricing_snapshot,
        )
        .await
    {
        Ok(r) => Some(r),
        Err(SessionBudgetError::BudgetExceeded { .. }) => {
            return IndependentReviewOutcome::refused(
                &provider_id,
                &model,
                "budget exceeded: cannot afford the independent review call",
            )
        }
        Err(e) => {
            return IndependentReviewOutcome::refused(
                &provider_id,
                &model,
                format!("budget unavailable for the review call: {e:?}"),
            )
        }
    };
    // The review attempt's durable provider-call START row is not written:
    // like every paid call of the runtime, ONE attempt-keyed terminal row is
    // recorded when the attempt TERMINATES (completed only when the exchange
    // settled; failed when a dispatched attempt ends without a settle), so a
    // crashed review reservation reconciles against exactly this attempt's
    // own completed row — never against a sibling logical op's merged row.
    let request = faktor_provider::GenericAgentRequest {
        model: model.clone(),
        system: REVIEW_MODEL_SYSTEM.to_string(),
        messages: vec![faktor_provider::RequestMessage {
            role: faktor_provider::Role::User,
            content: vec![faktor_provider::ContentPart::text(&prompt)],
        }],
        tools: vec![],
        max_output: Some(2048),
        reasoning: None,
        stream: true,
        meta: faktor_provider::RequestMeta {
            operation_id: op_id,
            session_id: session,
            provider: provider_id.clone(),
            attempt: 0,
            deadline_ms: REVIEW_MODEL_CALL_TIMEOUT.as_millis().min(u64::MAX as u128) as u64,
            cancellation: cancel.child(),
        },
    };
    // The review attempt's budget machine (attempt-accounting audit):
    // refund only pre-dispatch; ANY post-dispatch terminal outcome marks
    // the attempt UNCERTAIN; a clean paid-for exchange settles.
    let mut acct = crate::AttemptAccounting::new(deps.budgets.clone(), session, reservation);
    // P0-2: the durable dispatch marker is written immediately BEFORE the
    // provider request is sent — a crash after provider billing must
    // recover as UNCERTAIN, never as a $0 refund.
    if let Err(e) = acct.mark_dispatched().await {
        tracing::warn!(session = %session, "review reservation dispatch marker failed: {e}");
        // The provider was provably never contacted: release the
        // definitely-not-sent reservation before refusing.
        if let Err(refund_err) = acct.fail_before_dispatch().await {
            tracing::warn!(session = %session, "review reservation refund after a failed dispatch marker: {refund_err}");
        }
        return IndependentReviewOutcome::refused(
            &provider_id,
            &model,
            format!("the review call's reservation could not be durably marked dispatched: {e:?}"),
        );
    }
    let mut stream = provider.stream(request);
    let mut text = String::new();
    let mut complete = false;
    // P0-1 settlement truth: the LAST usage frame the stream carries (real
    // transports report one) is the settlement basis; without a frame the
    // documented chars/3 estimator stands in (interior calls never surface
    // frames through the verdict contract).
    let mut frame: Option<(u64, u64, u64, u64)> = None;
    let mut frame_reported: Option<u64> = None;
    let deadline = tokio::time::timeout(REVIEW_MODEL_CALL_TIMEOUT, async {
        use futures::StreamExt as _;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(faktor_provider::ProviderChunk::Text { text: t })
                | Ok(faktor_provider::ProviderChunk::Reasoning { text: t }) => {
                    text.push_str(&t);
                    if text.len() > REVIEW_MODEL_MAX_TEXT_CHARS {
                        // A verdict is small; anything bigger is not one.
                        complete = false;
                        return;
                    }
                }
                Ok(faktor_provider::ProviderChunk::Done) => {
                    complete = true;
                    return;
                }
                Ok(faktor_provider::ProviderChunk::Usage(usage)) => {
                    // Canonical usage (audit Phase-1 item C): the adapter
                    // split cache lines off the uncached input counter at
                    // its boundary — consume the categories directly (last
                    // frame wins, reasoning already rides the output line).
                    frame = Some((
                        usage.uncached_input_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens,
                        usage.output_tokens,
                    ));
                    if let Some(cost) = usage.reported_cost {
                        if let Some(micro) = authoritative_reported_micro(&cost) {
                            frame_reported = Some(micro);
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
        // Clean exhaustion after content: a clean end (same protocol as the
        // compaction summarizer).
        complete = !text.is_empty();
    });
    let _ = deadline.await;
    if !complete || text.trim().is_empty() {
        // The stream did NOT end cleanly (provider error / timeout / max
        // chars / cancellation) or produced nothing: a POST-dispatch
        // terminal outcome — the provider may have billed, so the machine
        // marks the attempt UNCERTAIN (never a refund; the reserved amount
        // keeps consuming until reconcile or the task-end finalize).
        if let Err(e) = acct
            .fail_after_dispatch(
                if complete {
                    "review_empty_verdict"
                } else {
                    "review_stream_failed"
                },
                None,
            )
            .await
        {
            tracing::warn!(session = %session, "review reservation uncertain marking failed: {e}");
        }
        // The failed review attempt's attempt-keyed provider-call row: this
        // attempt's failure with its own identity (no legacy logical-op
        // merged row). Failure-signal telemetry (P0-28): the review call did
        // not resolve — no verified sample (this site never learned a gate).
        if let Err(row_err) = handle.record_provider_call_attempt(
            attempt_identity,
            reservation.and_then(reservation_link),
            &provider_id,
            &model,
            "failed",
            None,
            None,
            Some("the review model produced no typed verdict"),
        ) {
            tracing::warn!(session = %session, "review provider-call failure row failed: {row_err}");
        }
        deps.routing.record_call_outcome(&SettledCallOutcome {
            provider: provider_id.clone(),
            model: model.clone(),
            phase: RouterPhase::Review,
            success: false,
            retried: false,
            rate_limited: false,
            latency_ms: 0,
            verified: None,
        });
        return IndependentReviewOutcome::refused(
            &provider_id,
            &model,
            "the review model produced no typed verdict",
        );
    }
    match faktor_verify::review::parse_review_verdict(&text) {
        Some(verdict) => {
            // Paid for a verdict: settle the review exchange against the
            // reservation's frozen price capture — the stream's last usage
            // frame when the transport reported one, else the documented
            // chars/3 token estimator. An unpriced reservation closes as a
            // documented Unknown spend, never a fabricated 1-micro-per-token
            // price. Errors are not fatal to the verdict.
            let (uncached_input, cache_read, cache_write, output) = frame.unwrap_or_else(|| {
                let in_est = prompt.len() as u64 / 3;
                let out_est = text.len() as u64 / 3;
                (in_est, 0, 0, out_est)
            });
            let mut settled = false;
            // The exchange completed cleanly: settle through the machine at
            // the frame/estimator actual. A refused settle (e.g. unknown
            // price under a hard cap) leaves NO dangling dispatched row —
            // the machine marks the attempt UNCERTAIN and the refusal is
            // not fatal to the verdict (the row resolves at task-end).
            if let Err(e) = acct
                .settle_usage(
                    uncached_input,
                    cache_read,
                    cache_write,
                    output,
                    frame_reported,
                    None,
                )
                .await
            {
                tracing::warn!(
                    session = %session,
                    "review reservation settlement refused: {e} (marking the attempt uncertain)"
                );
                if let Err(uncertain_err) = acct
                    .fail_after_dispatch("review_settle_refused", None)
                    .await
                {
                    tracing::warn!(
                        session = %session,
                        "review reservation uncertain marking after a refused settle: {uncertain_err}"
                    );
                }
            } else {
                settled = true;
            }
            // The settled review exchange's attempt-keyed provider-call row
            // with the canonical usage of the wave-B2 frame (input fold +
            // output — the row persists no cache split). Written only when
            // the reservation actually settled; a refused settle marks the
            // attempt UNCERTAIN and stays row-less for the finalize.
            if settled {
                if let Err(row_err) = handle.record_provider_call_attempt(
                    attempt_identity,
                    reservation.and_then(reservation_link),
                    &provider_id,
                    &model,
                    "completed",
                    Some(
                        uncached_input
                            .saturating_add(cache_read)
                            .saturating_add(cache_write),
                    ),
                    Some(output),
                    None,
                ) {
                    tracing::warn!(session = %session, "review provider-call completion row failed: {row_err}");
                }
            }
            // Telemetry outcome entry (P0-28): the settled Review call —
            // success=true with the actual provider/model. No verified
            // signal at this site (the deterministic gate decides below).
            deps.routing.record_call_outcome(&SettledCallOutcome {
                provider: provider_id.clone(),
                model: model.clone(),
                phase: RouterPhase::Review,
                success: settled,
                retried: false,
                rate_limited: false,
                latency_ms: 0,
                verified: None,
            });
            // Audit item 3: the reviewer output is admitted as a
            // verification-opinion `ModelOutput` (raw text + trust class +
            // the physical call's durable id). A trust refusal here is a
            // fail-closed refusal of the review itself.
            let output = ModelOutput::new(
                text.clone(),
                crate::OutputTrust::VerificationOpinion,
                attempt_identity.attempt_op_id.raw(),
            );
            IndependentReviewOutcome::with_verdict(&provider_id, &model, verdict, output)
                .unwrap_or_else(|refusal| {
                    IndependentReviewOutcome::refused(&provider_id, &model, refusal.to_string())
                })
        }
        None => {
            // Clean completion but no typed verdict: the exchange WAS paid
            // for (clean end with text) — settle at the frame/estimator
            // actual, then refuse (the review never silently clears).
            let (uncached_input, cache_read, cache_write, output) = frame.unwrap_or_else(|| {
                let in_est = prompt.len() as u64 / 3;
                let out_est = text.len() as u64 / 3;
                (in_est, 0, 0, out_est)
            });
            let mut settled = false;
            if let Err(e) = acct
                .settle_usage(
                    uncached_input,
                    cache_read,
                    cache_write,
                    output,
                    frame_reported,
                    None,
                )
                .await
            {
                tracing::warn!(
                    session = %session,
                    "review reservation settlement refused after an unparseable verdict: {e}"
                );
            } else {
                settled = true;
            }
            if settled {
                if let Err(row_err) = handle.record_provider_call_attempt(
                    attempt_identity,
                    reservation.and_then(reservation_link),
                    &provider_id,
                    &model,
                    "completed",
                    Some(
                        uncached_input
                            .saturating_add(cache_read)
                            .saturating_add(cache_write),
                    ),
                    Some(output),
                    None,
                ) {
                    tracing::warn!(session = %session, "review provider-call completion row failed: {row_err}");
                }
            }
            // Telemetry failure signal (P0-28): the review call resolved but
            // produced no typed verdict — the exchange is refused.
            deps.routing.record_call_outcome(&SettledCallOutcome {
                provider: provider_id.clone(),
                model: model.clone(),
                phase: RouterPhase::Review,
                success: false,
                retried: false,
                rate_limited: false,
                latency_ms: 0,
                verified: None,
            });
            IndependentReviewOutcome::refused(
                &provider_id,
                &model,
                format!(
                    "the review model output was not a typed verdict ({} chars)",
                    truncate(&text, 120)
                ),
            )
        }
    }
}

/// Gate reasons from an end-of-turn review value (audits 6/7: the
/// skeptical-review gate; audit 92 quality bar). Empty when the review is
/// absent or — under [`VerificationQuality::Normal`] — anything but an
/// exact verdict `"block"` (today's behavior: advisory suspects and
/// mislabeled/hostile verdict shapes never gate). Under Strict (the
/// mutating-turn default) ANY non-clean review gates: a `"block"` verdict,
/// a non-`"pass"` verdict shape (a `"weakened"`/`"advisory"` label must not
/// clear the gate), or advisory suspects on the changed code. Bounded:
/// each reason is truncated; a hostile review shape yields reasons naming
/// the shape — never an empty gate-clear.
pub(crate) fn review_blocking_reasons(
    review: Option<&serde_json::Value>,
    quality: VerificationQuality,
) -> Vec<OutcomeReason> {
    let Some(review) = review else {
        return Vec::new();
    };
    let verdict = review.get("verdict").and_then(|v| v.as_str());
    let blocking = review_strings(review.get("blocking"));
    let suspects = review_strings(review.get("suspects"));
    if quality == VerificationQuality::Normal {
        if verdict != Some("block") {
            return Vec::new();
        }
        return blocking
            .into_iter()
            .map(|b| OutcomeReason::new(ReasonCode::ReviewBlocked, truncate(&b, 200)))
            .collect();
    }
    // Strict: the review is clean ONLY when it says pass with nothing
    // listed. Everything else blocks — fail-closed for unknown verdicts.
    let clean = verdict == Some("pass") && blocking.is_empty() && suspects.is_empty();
    if clean {
        return Vec::new();
    }
    let mut out: Vec<OutcomeReason> = Vec::new();
    for b in blocking.into_iter().chain(suspects) {
        let detail = truncate(&b, 200);
        if !out.iter().any(|r: &OutcomeReason| r.detail == detail) {
            out.push(OutcomeReason::new(ReasonCode::ReviewBlocked, detail));
        }
    }
    if out.is_empty() {
        // A blocking/non-pass verdict that lists no reasons cannot clear
        // the gate: name the shape itself.
        out.push(OutcomeReason::new(
            ReasonCode::ReviewBlocked,
            format!(
                "review verdict {:?} is not a clean pass and lists no findings; a weakened or mislabeled review must not clear the gate",
                verdict.unwrap_or("<missing>")
            ),
        ));
    }
    out
}

/// The once-only acceptance-criteria ENTRIES (audit 25; wave 8 seed; typed
/// V2 audits 56/57/105; P0 criteria mandate): one entry per REQUIRED derived
/// check, each carrying its OWN typed [`CriterionBinding::RequiredCheck`]
/// (check id + command digest) — the canonical form that seeds BOTH the
/// typed `task` row's `acceptance_criteria` list and the `criteria`/`0`
/// memory fact (via [`criteria_canonical_text`]). The task GOAL is NOT
/// inserted as a proof obligation anymore: `Task.goal` stays human, and the
/// acceptance criteria are independently verifiable conditions. Legacy
/// `goal: ...` rows already persisted migrate deterministically through
/// `Criterion::effective_binding` (AggregateGoal, the independent final
/// review). None when the derivation produced no required check — nothing to
/// freeze. Every entry is bounded so the session layer's typed-criteria
/// validation can never reject a runtime-derived value. Each check entry is
/// a ProjectPolicy derivation tied to the derivation snapshot of its check
/// set, so a changed check set re-derives the stale criteria.
pub(crate) fn criteria_rows(_goal: &str, checks: &[faktor_verify::Check]) -> Option<Vec<String>> {
    let required: Vec<&faktor_verify::Check> = checks.iter().filter(|c| c.required).collect();
    if required.is_empty() {
        return None;
    }
    let snapshot = criteria_derivation_snapshot(&required);
    let mut criteria = Vec::with_capacity(required.len());
    for check in required {
        criteria.push(
            Criterion::derived(
                format!(
                    "required check: {}",
                    truncate(
                        &check.command,
                        faktor_session::task::MAX_TASK_CRITERION_TEXT_BYTES
                            - "required check: ".len()
                    )
                ),
                CriterionOrigin::ProjectPolicy,
                CriterionRequirement::Required,
                Some(snapshot.clone()),
            )
            .with_binding(CriterionBinding::RequiredCheck {
                check_id: check.id.clone(),
                command_digest: command_binding_digest(&check.command),
            }),
        );
    }
    Some(criteria.iter().map(Criterion::encode).collect())
}

/// Whether the `criteria`/`0` fact and the typed row's canonical text carry
/// the SAME criterion texts. Typed V2 entries decode to their human text
/// (JSON never contains a raw newline, so the canonical join stays one entry
/// per line); legacy plain entries are already text. This is the semantic
/// agreement the strict gate enforces — a representation migration is not a
/// divergence, a tampered criterion text is.
pub(crate) fn criteria_texts_agree(fact: &str, row_canonical_text: &str) -> bool {
    fn texts(canonical: &str) -> Vec<String> {
        canonical
            .split('\n')
            .map(Criterion::text_of)
            .collect::<Vec<_>>()
    }
    texts(fact) == texts(row_canonical_text)
}

/// Deterministic shortest-path planner over the task state machine (audit
/// P0-7): the legal `TaskTransition` edge sequence from `from` to `to`
/// (empty when the row already holds the target), or None when the machine
/// cannot connect the pair — terminal sources, and state pairs with no legal
/// path (NeedsVerification/Verifying cannot be Blocked or Running, so a
/// blocked gate after a failed attempt keeps NeedsVerification). The planner
/// never routes THROUGH a terminal state (VerifiedComplete/Failed/Cancelled
/// have no outgoing edges and must not be traversed).
pub(crate) fn task_route(from: TaskState, to: TaskState) -> Option<Vec<TaskTransition>> {
    use std::collections::{HashSet, VecDeque};
    if from == to {
        return Some(Vec::new());
    }
    if from.is_terminal() {
        return None;
    }
    let mut queue: VecDeque<(TaskState, Vec<TaskTransition>)> = VecDeque::new();
    let mut seen: HashSet<TaskState> = HashSet::new();
    seen.insert(from);
    queue.push_back((from, Vec::new()));
    while let Some((state, path)) = queue.pop_front() {
        for edge in TaskTransition::ALL {
            if !edge.legal_from(state) {
                continue;
            }
            let next = edge.to_state();
            if next.is_terminal() && next != to {
                continue;
            }
            let mut p = path.clone();
            p.push(edge);
            if next == to {
                return Some(p);
            }
            if seen.insert(next) {
                queue.push_back((next, p));
            }
        }
    }
    None
}

/// Translate a typed task-row refusal of a completion claim (audit P0-7/
/// P0-8) into the completion-gate outcome the genuine end reports: the claim
/// is NEVER silently certified, the row is never force-written, and the
/// reason names the typed cause. A revision mismatch (an external writer
/// moved the row between the record's certification and the completion
/// transaction) reuses [`ReasonCode::CriteriaInconsistent`]: the core code
/// table is frozen in this wave and the refusal IS a durable-rows
/// disagreement (the record certifies a revision the task row no longer
/// holds).
pub(crate) fn completion_refusal_gate(err: &TaskError) -> CompletionGate {
    CompletionGate::BlockedVerification {
        reasons: vec![OutcomeReason::new(
            ReasonCode::CriteriaInconsistent,
            format!(
                "the completion claim was refused by the typed task row: {err}; VerifiedComplete is never written without a passing record for the task's current revision"
            ),
        )],
    }
}

/// Rebuild one proof execution row from a settled background JOB row (audit
/// P0-5/26): the typed spec and the typed outcome both ride the durable job
/// row, so a record rebuilt at settlement carries the REAL program/argv/
/// exit/timestamps — never a re-split shell string, never a guess.
pub(crate) fn executed_from_job(
    mirror: &faktor_verify::Check,
    job: &faktor_session::VerificationJob,
) -> Option<(ExecutedCheck, bool)> {
    let spec: faktor_verify::exec::CheckSpec = serde_json::from_str(&job.spec_json).ok()?;
    let outcome: faktor_verify::exec::CheckOutcome =
        serde_json::from_str(job.result_json.as_ref()?).ok()?;
    let passed = outcome.status == faktor_verify::exec::CheckRunStatus::Passed;
    Some((executed_check_row(mirror, &spec, &outcome), passed))
}

/// The legacy kind of a job row (its durable `kind` tag — the job's spec
/// carries the same value at enqueue).
pub(crate) fn check_kind_of(job: &faktor_session::VerificationJob) -> faktor_verify::CheckKind {
    match job.kind.as_str() {
        "test" => faktor_verify::CheckKind::Test,
        "lint" => faktor_verify::CheckKind::Lint,
        _ => faktor_verify::CheckKind::Compile,
    }
}

/// Rebuild one attempt's mirrors/results from durable rows. A background
/// check whose job row is missing, Cancelled or Unavailable carries NO
/// verdict (unavailable, never a silent pass).
pub(crate) fn rebuild_attempt_verification(
    attempt: &faktor_session::VerificationAttempt,
    rows: &[faktor_session::VerificationJob],
) -> AttemptRebuild {
    let mut mirrors: Vec<faktor_verify::Check> = Vec::with_capacity(attempt.checks.len());
    let mut results: Vec<(String, bool)> = Vec::new();
    let mut unavailable: Vec<(String, String)> = Vec::new();
    let mut executed: Vec<ExecutedCheck> = Vec::new();
    for entry in &attempt.checks {
        let mirror = faktor_verify::Check {
            id: entry.check_id.clone(),
            kind: faktor_verify::CheckKind::Compile,
            command: entry.command.clone(),
            affects: Vec::new(),
            required: true,
        };
        mirrors.push(mirror.clone());
        match entry.inline {
            Some(faktor_session::VerificationInlineStatus::Passed) => {
                results.push((entry.check_id.clone(), true));
            }
            Some(faktor_session::VerificationInlineStatus::Failed) => {
                results.push((entry.check_id.clone(), false));
            }
            Some(faktor_session::VerificationInlineStatus::Unavailable) => {
                unavailable.push((entry.check_id.clone(), entry.command.clone()));
            }
            None => {
                let job = rows.iter().find(|j| j.check_id == entry.check_id);
                let settled = match job {
                    Some(j)
                        if matches!(
                            j.state,
                            faktor_session::VerificationJobState::Passed
                                | faktor_session::VerificationJobState::Failed
                        ) =>
                    {
                        let kind = check_kind_of(j);
                        let mut m = mirror.clone();
                        m.kind = kind;
                        executed_from_job(&m, j).map(|(e, ok)| {
                            executed.push(e);
                            results.push((entry.check_id.clone(), ok));
                        })
                    }
                    // Unavailable/Cancelled/missing: no verdict.
                    _ => None,
                };
                if settled.is_none() {
                    unavailable.push((entry.check_id.clone(), entry.command.clone()));
                }
            }
        }
    }
    AttemptRebuild {
        mirrors,
        results,
        unavailable,
        executed,
    }
}

impl ReadOnlyRepo for CandidateRepo {
    fn hash_file(&self, path: &str) -> Option<String> {
        self.0
            .hash_file_streaming(std::path::Path::new(path), None)
            .ok()
            .map(|(_, hash)| hash.to_hex())
    }
}

/// Evidence resolver over the attempt's OWN observed artifacts: check rows,
/// changed-file digests and the review's cited evidence strings. A reference
/// outside that universe does not resolve — never a guessed record.
pub(crate) struct AttemptEvidenceResolver {
    pub(crate) records: std::collections::BTreeMap<String, String>,
    pub(crate) snapshot: String,
}

impl AttemptEvidenceResolver {
    pub(crate) fn new(snapshot: &str) -> Self {
        Self {
            records: std::collections::BTreeMap::new(),
            snapshot: snapshot.to_string(),
        }
    }

    pub(crate) fn insert(&mut self, id: String, digest: String) {
        if !id.is_empty() && self.records.len() < 1024 {
            self.records.insert(id, digest);
        }
    }
}

impl EvidenceResolver for AttemptEvidenceResolver {
    fn resolve(&self, evidence_id: &str) -> Option<EvidenceRecord> {
        self.records.get(evidence_id).map(|digest| EvidenceRecord {
            evidence_id: evidence_id.to_string(),
            digest: digest.clone(),
            snapshot: self.snapshot.clone(),
        })
    }
}

/// The independent-review port over the recorded review value of an attempt.
/// The value was produced by the separate, context-isolated review phase;
/// this adapter maps it into the structured criterion-review contract. It
/// never synthesizes a verdict: an absent/unrecognized review is an error
/// (the evaluator degrades to Unavailable, never to a pass).
pub(crate) struct RecordedReviewPort {
    pub(crate) review: Option<serde_json::Value>,
}

impl IndependentReviewer for RecordedReviewPort {
    fn review<'a>(
        &'a self,
        request: &'a ReviewRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<StructuredReviewOutput, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(review) = &self.review else {
                return Err("no independent review ran for this attempt".into());
            };
            let verdict_text = review
                .get("verdict")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let verdict = match verdict_text {
                "pass" | "clean" => ReviewVerdict::Pass,
                "fail" | "block" | "concern" => ReviewVerdict::Fail,
                "unavailable" => ReviewVerdict::Unavailable,
                other => {
                    return Err(format!(
                        "reviewer verdict {other:?} is not a recognized structured verdict"
                    ))
                }
            };
            let strings = |key: &str| -> Vec<String> {
                review
                    .get(key)
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let findings = strings("findings");
            let mut evidence = strings("evidence");
            if evidence.is_empty() {
                // Reviewer-record mapping: the recorded review (the local
                // structured review record, or the review model's clean
                // verdict) lists no explicit refs, so bind the verdict to
                // the attempt's OWN observed artifacts instead — every ref
                // resolves through the same attempt evidence resolver and
                // carries the same candidate snapshot. A reviewer pass still
                // never validates without resolving evidence.
                for row in &request.check_outcomes {
                    if !row.check_id.is_empty() {
                        evidence.push(row.check_id.clone());
                    }
                }
                for entry in &request.change_set {
                    if !entry.path.is_empty() {
                        evidence.push(format!("file:{}", entry.path));
                    }
                }
            }
            let explanation = if findings.is_empty() {
                format!("independent reviewer verdict {verdict_text:?}")
            } else {
                truncate(&findings.join("; "), 2000)
            };
            Ok(StructuredReviewOutput {
                criterion_id: request.criterion_id.clone(),
                snapshot: request.candidate_snapshot.clone(),
                verdict,
                evidence_refs: evidence
                    .into_iter()
                    .take(faktor_verify::criteria::MAX_REVIEW_EVIDENCE_REFS)
                    .collect(),
                explanation,
            })
        })
    }
}

/// Evaluate every acceptance criterion of one attempt through its OWN typed
/// binding. Check-bound criteria resolve against the executed check rows,
/// file-state bindings hash the VERIFIED CANDIDATE, evidence bindings resolve
/// immutable evidence, and IndependentReview/AggregateGoal verdicts come from
/// the recorded independent review — a reviewer pass with zero evidence is
/// refused by the evaluator.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn criterion_verdicts_from_attempt(
    criteria: &[String],
    checks: &[faktor_verify::Check],
    results: &[(String, bool)],
    unavailable: &[(String, String)],
    changed: &[String],
    ws: &faktor_fs::WorkspaceHandle,
    // Audit item 3: review evidence is ADMITTED (typed) before it can feed
    // any criterion verdict — a caller cannot hand raw model text here.
    review: Option<&ReviewEvidence>,
    candidate_snapshot: &str,
    goal: &str,
) -> Vec<CriterionVerification> {
    if criteria.is_empty() {
        return Vec::new();
    }
    let review = review.map(ReviewEvidence::as_value);
    let typed = decode_criteria(criteria);
    let mut resolver = AttemptEvidenceResolver::new(candidate_snapshot);
    let mut check_rows: Vec<CheckOutcomeRow> = Vec::new();
    for check in checks {
        let status = if let Some((_, ok)) = results.iter().find(|(id, _)| id == &check.id) {
            if *ok {
                CheckOutcomeStatus::Passed
            } else {
                CheckOutcomeStatus::Failed
            }
        } else if unavailable.iter().any(|(id, _)| id == &check.id) {
            CheckOutcomeStatus::Unavailable
        } else {
            continue;
        };
        let digest = command_binding_digest(&check.command);
        resolver.insert(check.id.clone(), digest.clone());
        check_rows.push(CheckOutcomeRow {
            check_id: check.id.clone(),
            command_digest: digest,
            status,
            evidence: Some(truncate(
                &check.command,
                faktor_session::MAX_VERIFICATION_EVIDENCE_BYTES,
            )),
        });
    }
    let mut change_set: Vec<ChangeSetEntry> = Vec::new();
    for path in changed.iter().take(64) {
        let digest = ws
            .hash_file_streaming(std::path::Path::new(path), None)
            .ok()
            .map(|(_, h)| h.to_hex())
            .unwrap_or_default();
        if !digest.is_empty() {
            resolver.insert(format!("file:{path}"), digest.clone());
            resolver.insert(path.clone(), digest.clone());
        }
        change_set.push(ChangeSetEntry {
            path: path.clone(),
            digest_hex: digest,
            status: ChangeSetStatus::Modified,
        });
    }
    if let Some(review) = review {
        if let Some(evidence) = review.get("evidence").and_then(|v| v.as_array()) {
            for cite in evidence.iter().filter_map(|s| s.as_str()).take(64) {
                resolver.insert(cite.to_string(), command_binding_digest(cite));
            }
        }
    }
    let evidence: std::sync::Arc<dyn EvidenceResolver> = std::sync::Arc::new(resolver);
    let reviewer: std::sync::Arc<dyn IndependentReviewer> =
        std::sync::Arc::new(RecordedReviewPort {
            review: review.cloned(),
        });
    let repo: std::sync::Arc<dyn ReadOnlyRepo> = std::sync::Arc::new(CandidateRepo(ws.clone()));
    let evaluator = FallbackEvaluator;
    let mut evaluations: Vec<(usize, CriterionEvaluation)> = Vec::new();
    let context_of = |id: String, subordinate: Vec<CriterionEvaluationRow>| {
        let mut ctx =
            CriterionEvaluationContext::new(id, candidate_snapshot.to_string(), evidence.clone());
        ctx.goal = goal.to_string();
        ctx.change_set = change_set.clone();
        ctx.checks = check_rows.clone();
        ctx.repo = Some(repo.clone());
        ctx.reviewer = Some(reviewer.clone());
        ctx.subordinate = subordinate;
        ctx
    };
    for (i, c) in typed.iter().enumerate() {
        if matches!(c.effective_binding(), CriterionBinding::AggregateGoal) {
            continue;
        }
        let criterion = faktor_verify::criteria::Criterion::new(
            c.id.to_string(),
            c.text.clone(),
            Some(c.effective_binding()),
        );
        let mut ctx = context_of(c.id.to_string(), Vec::new());
        evaluations.push((i, evaluator.evaluate(&mut ctx, &criterion).await));
    }
    for (i, c) in typed.iter().enumerate() {
        if !matches!(c.effective_binding(), CriterionBinding::AggregateGoal) {
            continue;
        }
        let criterion = faktor_verify::criteria::Criterion::new(
            c.id.to_string(),
            c.text.clone(),
            Some(c.effective_binding()),
        );
        let subordinate: Vec<CriterionEvaluationRow> = evaluations
            .iter()
            .map(|(j, evaluation)| CriterionEvaluationRow {
                criterion_id: typed[*j].id.to_string(),
                binding: Some(typed[*j].effective_binding()),
                evaluation: evaluation.clone(),
            })
            .collect();
        let mut ctx = context_of(c.id.to_string(), subordinate);
        evaluations.push((i, evaluator.evaluate(&mut ctx, &criterion).await));
    }
    evaluations.sort_by_key(|(i, _)| *i);
    evaluations
        .into_iter()
        .map(|(i, evaluation)| CriterionVerification {
            criterion_key: criteria[i].clone(),
            passed: evaluation.is_passed(),
            evidence: {
                let refs = evaluation.evidence_refs();
                if refs.is_empty() {
                    evaluation.reason().map(|reason| {
                        truncate(reason, faktor_session::MAX_VERIFICATION_EVIDENCE_BYTES)
                    })
                } else {
                    Some(truncate(
                        &refs.join("; "),
                        faktor_session::MAX_VERIFICATION_EVIDENCE_BYTES,
                    ))
                }
            },
            binding: Some(typed[i].effective_binding()),
        })
        .collect()
}

/// Assemble the durable-proof payload of one verification attempt with a
/// verdict (audit P0-8 + typed migration P0-9/10; see [`VerificationProof`]):
/// - one [`CheckExecution`] per required check that RAN, built from the
///   typed [`ExecutedCheck`] rows: the check id, the TYPED program + args
///   (bounded: the derivation caps commands at 512 chars, the session layer
///   at 32 args of 1024 bytes), a category derived from the check kind,
///   `required = true`, the pass/fail status, the REAL exit code (Some(0)
///   pass; a scripted failure has no exit), the bounded output summary and
///   the real started/finished timestamps. Background-job attempts rebuild
///   these rows from the durable job rows at settlement
///   ([`executed_from_job`] — job results only, audit P0-5/26);
/// - one [`CriterionVerification`] per acceptance-criteria entry (the SAME
///   `criteria_rows` texts that seed the typed task row, so the completion
///   coverage check compares identical keys): the goal entry passes unless a
///   required check failed, each required-check entry passes on its result
///   and fails with evidence when it failed or produced no verdict;
/// - bounded content-addressed changed-file evidence: whole-file streaming
///   BLAKE3 digests through the workspace handle (traversal-safe; unreadable
///   files are skipped, exactly like review heads).
///
/// Attempts whose required checks produced NO verdict (only unavailable
/// ones) never reach this builder — there is nothing a record could certify.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn verification_proof_from_attempt(
    criteria: Option<&[String]>,
    checks: &[faktor_verify::Check],
    results: &[(String, bool)],
    unavailable: &[(String, String)],
    runs: &[ExecutedCheck],
    changed: &[String],
    ws: &faktor_fs::WorkspaceHandle,
    // Audit item 3: the review value must already be ADMITTED evidence
    // (verification-opinion provenance); a raw model value cannot reach the
    // proof.
    review: Option<&ReviewEvidence>,
) -> Result<VerificationProof, String> {
    let Some(candidate_snapshot) = root_snapshot_best_effort(ws) else {
        return Err(
            "the verification root has no provable content identity (special/oversized tree); \
             refusing to mint proof against a synthetic snapshot"
                .to_string(),
        );
    };
    let mut executions = Vec::new();
    for run in runs {
        let category = match run.kind {
            faktor_verify::CheckKind::Compile => "compile",
            faktor_verify::CheckKind::Test => "test",
            faktor_verify::CheckKind::Lint => "lint",
        };
        executions.push(CheckExecution {
            check: truncate(&run.id, faktor_session::MAX_VERIFICATION_CHECK_NAME_BYTES),
            program: truncate(&run.program, faktor_session::MAX_VERIFICATION_PROGRAM_BYTES),
            args: run
                .args
                .iter()
                .take(faktor_session::MAX_VERIFICATION_CHECK_ARGS)
                .map(|a| truncate(a, faktor_session::MAX_VERIFICATION_CHECK_ARG_BYTES))
                .collect(),
            category: category.into(),
            required: true,
            status: if run.passed {
                VerificationStatus::Passed
            } else {
                VerificationStatus::Failed
            },
            started_ms: run.started_ms,
            finished_ms: Some(run.finished_ms),
            exit: run.exit,
            summary: run.summary.clone(),
        });
    }
    let entries: &[String] = criteria.unwrap_or(&[]);
    let verdicts = criterion_verdicts_from_attempt(
        entries,
        checks,
        results,
        unavailable,
        changed,
        ws,
        review,
        &candidate_snapshot,
        "",
    )
    .await;
    let mut files = Vec::new();
    for path in changed.iter().take(16) {
        if let Ok((size, hash)) = ws.hash_file_streaming(std::path::Path::new(path), None) {
            files.push(FileStateEvidence {
                path: truncate(path, faktor_session::MAX_VERIFICATION_PATH_BYTES),
                digest_hex: hash.to_hex(),
                size,
            });
        }
    }
    let reviewer = match review.map(ReviewEvidence::as_value) {
        Some(v)
            if serde_json::to_string(v)
                .is_ok_and(|s| s.len() <= faktor_session::MAX_VERIFICATION_REVIEWER_JSON_BYTES) =>
        {
            Some(v.clone())
        }
        _ => None,
    };
    Ok(VerificationProof {
        checks: executions,
        criteria: verdicts,
        changed_files: files,
        review: reviewer,
    })
}

impl AgentRuntime {
    /// Force the verification/review quality for every subsequent turn
    /// (audit 92). Unset (the default) means: mutating turns run
    /// [`VerificationQuality::Strict`], non-mutating turns run Normal —
    /// the completion-proof spec's rules bind mutating coding tasks, so the
    /// strict bar is the default exactly there. Setting Normal restores
    /// today's behavior everywhere.
    pub fn set_verification_quality(&self, quality: VerificationQuality) {
        let mode: u8 = match quality {
            VerificationQuality::Normal => 1,
            VerificationQuality::Strict => 2,
        };
        self.quality_mode
            .store(mode, std::sync::atomic::Ordering::SeqCst);
    }

    /// The quality in effect for a turn that changed `mutated` files.
    pub(crate) fn quality_for_turn(&self, mutated: bool) -> VerificationQuality {
        match self.quality_mode.load(std::sync::atomic::Ordering::SeqCst) {
            1 => VerificationQuality::Normal,
            2 => VerificationQuality::Strict,
            // Unset: the spec's rules bind mutating coding tasks.
            _ => {
                if mutated {
                    VerificationQuality::Strict
                } else {
                    VerificationQuality::Normal
                }
            }
        }
    }

    /// One-time migration of a legacy `VerifyHash` recovery row (audit
    /// P1-F). The recorded path is only ever a claim to be PROVEN: the
    /// session's durable workspace root is resolved, the old path is
    /// canonicalized to prove it is inside that root, and the proof is
    /// converted to a normalized workspace-relative path. Only then is the
    /// still-running row annotated with the modern [`FilePostcondition`]
    /// (durable, BEFORE any read), so a crash after this write resumes
    /// through the handle-relative path exactly once and never re-derives
    /// capability from the legacy string. `None` = containment cannot be
    /// proven (`..` climbs, symlinks pointing out, different roots,
    /// unreadable roots, foreign path grammars): the caller classifies the
    /// effect Unknown/NeedsUserInput, NEVER Verified.
    pub(crate) fn migrate_legacy_verify_row(
        &self,
        handle: &faktor_session::SessionHandle,
        row: &ToolRunRow,
        legacy_path: &str,
        expected: FileHash,
    ) -> faktor_core::Result<Option<FilePostcondition>> {
        let Some(root) = self.deps.session.resolve_workspace_root(handle.id())? else {
            return Ok(None);
        };
        let identity = handle.identity()?;
        let ws = match self.deps.workspaces.open(identity.workspace_id, root) {
            Ok(ws) => ws,
            Err(_) => return Ok(None),
        };
        let Some(relative) = legacy_relative_path_within(ws.root(), legacy_path) else {
            return Ok(None);
        };
        if workspace_relative_path_rejection(&relative).is_some() {
            return Ok(None);
        }
        let postcondition = FilePostcondition {
            workspace_id: identity.workspace_id,
            worktree_id: identity.worktree_id,
            relative_path: relative,
            expected_hash: expected,
        };
        let raw = serde_json::to_value(&postcondition)
            .map_err(|e| Error::malformed(format!("legacy postcondition serialization: {e}")))?;
        handle.record_tool_postcondition(row.op_id, &raw)?;
        Ok(Some(postcondition))
    }

    /// Verify a workspace write through the WorkspaceFileService: canonical
    /// safe resolution of the RELATIVE path against the session's effective
    /// workspace root (P0-48 root re-pointing: a live shadow of `session`
    /// verifies against the shadow root — a crash mid-drive re-verifies the
    /// file exactly where the write landed; no `..`, no symlink escapes,
    /// never the daemon cwd), then a BLAKE3 read through the handle's OWN
    /// anchored relative open — never a hash of a pathname re-resolved after
    /// the gate (audit P1-F removed the last raw-path hash from recovery;
    /// the post-open identity net catches an entry swapped after
    /// resolution). `None` when the file is missing or unreadable: the
    /// honest "write never landed" — never Verified.
    pub(crate) fn verify_workspace_file(
        &self,
        session_id: SessionId,
        pc: &FilePostcondition,
    ) -> faktor_core::Result<Option<FileHash>> {
        let Some(root) = self.deps.session.resolve_workspace_root(session_id)? else {
            return Err(Error::malformed(format!(
                "tool recovery: workspace {} is not registered",
                pc.workspace_id
            )));
        };
        let ws = self
            .deps
            .workspaces
            .open(pc.workspace_id, root)
            .map_err(|e| Error::malformed(format!("tool recovery workspace open: {e}")))?;
        // Traversal/symlink-unsafe relative paths are REJECTED loudly here —
        // recovery never touches a file outside the workspace root. The
        // host-side grammar runs FIRST: absolute Windows shapes (drive
        // letters, UNC/device prefixes) are refused on every platform,
        // exactly like Unix-absolute paths — the platform resolver must
        // never be the only gate.
        if let Some(reason) = workspace_relative_path_rejection(&pc.relative_path) {
            return Err(Error::permission(format!(
                "tool recovery path {:?} rejected: {reason}",
                pc.relative_path
            )));
        }
        ws.resolve(std::path::Path::new(&pc.relative_path))
            .map_err(|e| {
                Error::permission(format!(
                    "tool recovery path {:?} rejected: {e}",
                    pc.relative_path
                ))
            })?;
        match ws.hash_file_streaming(std::path::Path::new(&pc.relative_path), None) {
            Ok((_bytes, actual)) => Ok(Some(actual)),
            Err(_) => Ok(None),
        }
    }

    /// The failure identity of one durable verification record, computed
    /// EXACTLY like [`Self::learning_episode_from_record`] (kind
    /// `verification_failure`, code = the failed check, message = its
    /// summary) so the fingerprint matches the mined learning's stored
    /// failure identity. `None` without a check — no honest match.
    pub(crate) fn record_failure_fingerprint(
        record: &faktor_session::VerificationRecord,
    ) -> Option<faktor_learning::FailureFingerprint> {
        use faktor_learning::{FailureDescriptor, FailureFingerprint};

        let failed_check = record
            .checks
            .iter()
            .find(|check| check.status == VerificationStatus::Failed)
            .or_else(|| record.checks.first())?;
        FailureDescriptor::new(
            "verification_failure",
            Some(&failed_check.check),
            failed_check
                .summary
                .as_deref()
                .unwrap_or(failed_check.check.as_str()),
        )
        .ok()
        .map(|descriptor| FailureFingerprint::of(&descriptor))
    }

    /// Record the newest failed attempt as a durable UNVERIFIED episode. No
    /// learning is minted here (the miner refuses recovery-less and
    /// unverified episodes), so a failure with no later verified recovery
    /// stays learning-free.
    pub(crate) fn record_failed_attempt(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Result<(), faktor_learning::LearningError> {
        let Some(record) = Self::latest_attempt_record(handle, VerificationStatus::Failed) else {
            return Ok(());
        };
        let Some(episode) = self.learning_episode_from_record(handle, &record) else {
            return Ok(());
        };
        let mut store = faktor_learning::SessionLearningStore::open(
            handle.clone(),
            faktor_learning::DEFAULT_MEMORY_CAPACITY,
        )?;
        store.record_episode(&episode)
    }

    /// Run the REAL shared verification on an ORCHESTRATED run's ACTUAL
    /// final integration root (P0 orchestrated-completion binding): the
    /// derived check set comes from the multi-component project profile
    /// detected over `root` (never a synthetic one-check-per-criterion
    /// aggregate), the checks execute through the wired
    /// [`crate::VerificationService`] under policy budgets, and each parent
    /// acceptance criterion is certified only when the whole required check
    /// set passed.
    ///
    /// Typed refusals (never a silent PASS): a disabled service, an empty
    /// repo map, a derivation cap refusal, no derived check applying to the
    /// integrated change, or any check that could not produce a verdict all
    /// return `Err`/`Unavailable` — the caller refuses completion instead of
    /// minting proof from nothing. The checks run against `root` exactly
    /// (the caller passes the integration root), never the daemon cwd.
    pub async fn verify_integrated_root(
        &self,
        handle: &faktor_session::SessionHandle,
        root: &std::path::Path,
        changed: &[String],
        criteria: &[String],
        cancel: &CancellationToken,
    ) -> Result<IntegratedRootVerification, String> {
        let service = self.deps.verification.clone();
        if service.is_disabled() {
            return Err(
                "no verifier configured (no objective mechanism for this deployment)".into(),
            );
        }
        if !root.is_dir() {
            return Err(format!(
                "integration root {} is not a directory",
                root.display()
            ));
        }
        let row = handle
            .row()
            .map_err(|e| format!("session row unresolvable: {e}"))?;
        // The integrated root is a daemon-owned CANDIDATE directory
        // (`run_exec_dir/candidate`), NOT the session's durable workspace.
        // Opening it under the session's workspace id always conflicted with
        // the live handle ("workspace N already open at ...") and left every
        // orchestrated run unverified.
        let ws = self
            .deps
            .workspaces
            .open_ephemeral(root.to_path_buf())
            .map_err(|e| format!("integration root could not be opened: {e}"))?;
        let Some(candidate_snapshot) = root_snapshot_best_effort(&ws) else {
            return Err(format!(
                "the integrated root {} has no provable content identity (special/oversized tree); \
                 refusing to verify against a synthetic snapshot",
                root.display()
            ));
        };
        // P2-VERIFY: the goal is an INPUT of the review/verification; an
        // unreadable task row must refuse the verification, never feed an
        // empty goal into a passing verdict.
        let goal = handle
            .get_task(row.task_id)
            .map_err(|e| format!("task row of the integrated root is unreadable: {e}"))?
            .map(|t| t.goal)
            .unwrap_or_default();
        // An EMPTY aggregate change set derives no checks: the ONLY objective
        // mechanism left is the independent reviewer's no-op proof. It is a
        // real review-model call over the candidate snapshot; when no
        // reviewer can produce a typed verdict the caller refuses completion
        // (never a synthetic pass from an empty suite).
        if changed.is_empty() {
            return self
                .verify_no_op_root(handle, &ws, criteria, &candidate_snapshot, &goal, cancel)
                .await;
        }
        // Bounded repository discovery with an EXPLICIT completeness verdict
        // (audit: verification must not certify a partial repo view). A
        // non-Complete inventory can never authorize a passing suite: the
        // derivation input would silently miss manifests/sources beyond a
        // cap, so the whole integrated-root verification is Unavailable with
        // the typed reason.
        let inventory = match &self.deps.supervisor {
            Some(supervisor) => faktor_verify::discover_repo_inventory_with_supervisor(
                root,
                &faktor_verify::InventoryBudget::default(),
                supervisor,
            ),
            None => faktor_verify::discover_repo_inventory(root),
        };
        let repo_files = inventory.files.clone();
        if let Some(reason) = inventory.refusal_reason() {
            let criteria_rows = criterion_verdicts_from_attempt(
                criteria,
                &[],
                &[],
                &[],
                changed,
                &ws,
                None,
                &candidate_snapshot,
                &goal,
            )
            .await;
            return Ok(IntegratedRootVerification {
                status: VerificationStatus::Unavailable,
                checks: Vec::new(),
                criteria: criteria_rows,
                changed: changed.to_vec(),
                summary: format!(
                    "integrated-root verification unavailable: repository inventory incomplete ({reason})"
                ),
                review_model_identity: None,
            });
        }
        if repo_files.is_empty() {
            return Err("repository file map empty (no project type detectable)".into());
        }
        let profile = faktor_verify::derive::detect_project_profile(root, &repo_files);
        let changed_paths: Vec<std::path::PathBuf> =
            changed.iter().map(std::path::PathBuf::from).collect();
        let specs = faktor_verify::derive::derive_checks(&profile, &changed_paths)
            .map_err(|e| format!("verification derivation refused: {e}"))?;
        if specs.is_empty() {
            return Err("no derived checks apply to the integrated change".into());
        }
        let checks: Vec<faktor_verify::Check> = specs.iter().map(legacy_mirror_of_spec).collect();
        let base_ctx = faktor_verify::exec::VerificationContext {
            session_id: handle.id().raw(),
            task_id: row.task_id.raw(),
            operation_id: self
                .deps
                .session
                .try_next_op_id()
                .map_err(|e| format!("verification op-id allocation failed: {e}"))?
                .raw(),
            workspace_id: row.workspace_id.raw(),
            worktree_id: row.worktree_id.raw(),
            root: root.to_path_buf(),
            deadline: std::time::Instant::now(),
            cancellation: cancel.child(),
        };
        let mut results: Vec<(String, bool)> = Vec::new();
        let mut executed: Vec<CheckExecution> = Vec::new();
        let mut unavailable: Vec<(String, String)> = Vec::new();
        for spec in specs.iter().filter(|s| s.required) {
            let budget = match service.budget_for(spec) {
                BudgetDecision::RunInline(budget) => budget,
                BudgetDecision::RunAsTaskOwnedOperation => service.policy().unit_max,
            };
            if budget.is_zero() {
                unavailable.push((spec.id.clone(), spec.program.to_string_lossy().into_owned()));
                continue;
            }
            let mut vctx = base_ctx.clone();
            vctx.deadline = std::time::Instant::now() + budget;
            let outcome = service.execute(spec, &vctx).await;
            let status = match outcome.status {
                CheckRunStatus::Passed => VerificationStatus::Passed,
                CheckRunStatus::Failed => VerificationStatus::Failed,
                CheckRunStatus::Unavailable => VerificationStatus::Unavailable,
            };
            match outcome.status {
                CheckRunStatus::Passed => results.push((spec.id.clone(), true)),
                CheckRunStatus::Failed => results.push((spec.id.clone(), false)),
                CheckRunStatus::Unavailable => {
                    unavailable
                        .push((spec.id.clone(), spec.program.to_string_lossy().into_owned()));
                }
            }
            executed.push(CheckExecution {
                check: spec.id.clone(),
                program: spec.program.to_string_lossy().into_owned(),
                args: spec
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
                category: format!("{:?}", spec.category).to_ascii_lowercase(),
                required: spec.required,
                status,
                started_ms: outcome.started_ms,
                finished_ms: Some(outcome.finished_ms),
                exit: outcome.exit,
                summary: outcome.summary.clone(),
            });
        }
        let status = match faktor_verify::acceptance(&checks, &results) {
            faktor_verify::Acceptance::Pass => VerificationStatus::Passed,
            faktor_verify::Acceptance::Fail => VerificationStatus::Failed,
            faktor_verify::Acceptance::Pending => VerificationStatus::Unavailable,
        };
        let summary = format!(
            "integrated-root verification: {} required check(s) derived ({} executed, {} unavailable, {} failed); root {}",
            checks.iter().filter(|c| c.required).count(),
            results.len(),
            unavailable.len(),
            results.iter().filter(|(_, ok)| !ok).count(),
            root.display()
        );
        // The WIRED independent-reviewer port (P0 criteria mandate): the
        // orchestrated root's own review phase runs over the CANDIDATE (the
        // same separate, context-isolated review contract the turn path
        // uses; risky changes force the real review-model call) and its
        // recorded value answers every AggregateGoal/IndependentReview
        // criterion through [`RecordedReviewPort`]. With no reviewer record
        // the criteria stay Unavailable — an honest absence that blocks
        // completion when the criterion is required, never a pass.
        let review = independent_completion_review(
            self.deps.as_ref(),
            handle,
            &ws,
            changed,
            &goal,
            &repo_files,
            cancel,
        )
        .await;
        // Typed criterion verdicts (P0): evaluated through each criterion's
        // OWN binding. The blanket `passed = integrated checks passed`
        // mapping is GONE; a criterion without a binding the evaluator can
        // resolve is Unavailable, never Passed. Audit item 3: the review
        // value is ADMITTED before it can back any criterion verdict.
        let review_evidence = admit_review_evidence(review.as_ref());
        let criteria_rows = criterion_verdicts_from_attempt(
            criteria,
            &checks,
            &results,
            &unavailable,
            changed,
            &ws,
            review_evidence.as_ref(),
            &candidate_snapshot,
            &goal,
        )
        .await;
        Ok(IntegratedRootVerification {
            status,
            checks: executed,
            criteria: criteria_rows,
            changed: changed.to_vec(),
            summary,
            review_model_identity: review.as_ref().and_then(review_model_identity_of),
        })
    }

    /// Attempt-based twin of [`AgentRuntime::verify_integrated_root`] (audit
    /// P0-5/26 production wiring): the shared derivation runs over the SAME
    /// candidate root, cheap required checks execute INLINE, and expensive
    /// `RunAsTaskOwnedOperation` checks become durable Queued jobs of the
    /// exact `attempt_op`. While any job is open the method returns
    /// `pending = true` (the caller leaves the run unverified); a later call
    /// with the SAME `attempt_op` consumes the EXACT attempt — never
    /// "whatever attempt is newest". When the exact attempt was superseded
    /// by a newer one, its open jobs are cancelled and a FRESH attempt is
    /// begun: late results for the old attempt are structurally refused by
    /// the store and can never resolve the new one. The daemon verification
    /// executor is the normal resolver of the queued jobs.
    #[allow(clippy::too_many_arguments)]
    pub async fn verify_integrated_root_attempt(
        &self,
        handle: &faktor_session::SessionHandle,
        root: &std::path::Path,
        changed: &[String],
        criteria: &[String],
        cancel: &CancellationToken,
        attempt_op: u64,
    ) -> Result<IntegratedRootAttemptOutcome, String> {
        if attempt_op == 0 {
            return Err("root verification attempt op must be non-zero".into());
        }
        let service = self.deps.verification.clone();
        if service.is_disabled() {
            return Err(
                "no verifier configured (no objective mechanism for this deployment)".into(),
            );
        }
        // Scripted/embedded backends cannot persist jobs: the inline path is
        // byte-identical to [`Self::verify_integrated_root`].
        if !service.can_persist_jobs() {
            let verification = self
                .verify_integrated_root(handle, root, changed, criteria, cancel)
                .await?;
            return Ok(IntegratedRootAttemptOutcome {
                attempt_op,
                pending: false,
                verification: Some(verification),
            });
        }
        if !root.is_dir() {
            return Err(format!(
                "integration root {} is not a directory",
                root.display()
            ));
        }
        let row = handle
            .row()
            .map_err(|e| format!("session row unresolvable: {e}"))?;
        let task_id = row.task_id;
        let ws = self
            .deps
            .workspaces
            .open(row.workspace_id, root.to_path_buf())
            .map_err(|e| format!("integration root could not be opened: {e}"))?;
        let Some(candidate_snapshot) = root_snapshot_best_effort(&ws) else {
            return Err(format!(
                "the integrated root {} has no provable content identity (special/oversized tree); \
                 refusing to verify against a synthetic snapshot",
                root.display()
            ));
        };
        // P2-VERIFY: same rule as the fresh path — the task row is a review
        // input, and a read failure refuses the attempt.
        let goal = handle
            .get_task(task_id)
            .map_err(|e| format!("task row of the integrated root is unreadable: {e}"))?
            .map(|t| t.goal)
            .unwrap_or_default();
        if changed.is_empty() {
            let verification = self
                .verify_no_op_root(handle, &ws, criteria, &candidate_snapshot, &goal, cancel)
                .await?;
            return Ok(IntegratedRootAttemptOutcome {
                attempt_op,
                pending: false,
                verification: Some(verification),
            });
        }
        let inventory = match &self.deps.supervisor {
            Some(supervisor) => faktor_verify::discover_repo_inventory_with_supervisor(
                root,
                &faktor_verify::InventoryBudget::default(),
                supervisor,
            ),
            None => faktor_verify::discover_repo_inventory(root),
        };
        let repo_files = inventory.files.clone();
        if let Some(reason) = inventory.refusal_reason() {
            let criteria_rows = criterion_verdicts_from_attempt(
                criteria,
                &[],
                &[],
                &[],
                changed,
                &ws,
                None,
                &candidate_snapshot,
                &goal,
            )
            .await;
            return Ok(IntegratedRootAttemptOutcome {
                attempt_op,
                pending: false,
                verification: Some(IntegratedRootVerification {
                    status: VerificationStatus::Unavailable,
                    checks: Vec::new(),
                    criteria: criteria_rows,
                    changed: changed.to_vec(),
                    summary: format!(
                        "integrated-root verification unavailable: repository inventory incomplete ({reason})"
                    ),
                    review_model_identity: None,
                }),
            });
        }
        // The EXACT attempt exists: consume it (or supersede it).
        if let Some(attempt) = handle
            .verification_attempt(task_id.raw(), attempt_op)
            .map_err(|e| format!("verification attempt read: {e}"))?
        {
            let newest = handle
                .current_verification_attempt(task_id.raw())
                .map_err(|e| format!("verification attempt read: {e}"))?;
            if newest.as_ref().map(|a| a.op_id) != Some(attempt.op_id) {
                // A newer attempt exists: this one is structurally frozen.
                // Cancel its open jobs and begin a FRESH attempt under a new
                // op; only the fresh attempt may ever settle this run.
                let note = format!(
                    "superseded by the newer verification attempt {:?}",
                    newest.as_ref().map(|a| a.op_id)
                );
                // Supersede cleanup inside a String-error function: recorded
                // (marker + audit), never masked into a different error.
                self.dw_note_cancel_verification_attempt(
                    handle,
                    task_id.raw(),
                    attempt.op_id,
                    &note,
                    DW_SITE_ROOT_ATTEMPT_CANCEL,
                );
                let fresh = self
                    .deps
                    .session
                    .try_next_op_id()
                    .map_err(|e| format!("verification op-id allocation failed: {e}"))?
                    .raw();
                return self
                    .begin_integrated_root_attempt(
                        handle,
                        root,
                        changed,
                        criteria,
                        cancel,
                        fresh,
                        &ws,
                        &candidate_snapshot,
                        &goal,
                        &repo_files,
                    )
                    .await;
            }
            let rows = handle
                .verification_attempt_jobs(task_id.raw(), attempt.op_id)
                .map_err(|e| format!("verification job rows: {e}"))?;
            if rows.iter().any(|j| j.state.is_open()) {
                // The exact attempt is still executing (the daemon executor
                // owns it): the run must NOT re-derive and must NOT land.
                return Ok(IntegratedRootAttemptOutcome {
                    attempt_op,
                    pending: true,
                    verification: None,
                });
            }
            let review = independent_completion_review(
                self.deps.as_ref(),
                handle,
                &ws,
                changed,
                &goal,
                &repo_files,
                cancel,
            )
            .await;
            let rebuild = rebuild_attempt_verification(&attempt, &rows);
            let verification = self
                .root_verification_from_rebuild(
                    root,
                    criteria,
                    changed,
                    &ws,
                    &candidate_snapshot,
                    &goal,
                    review.as_ref(),
                    rebuild,
                )
                .await;
            return Ok(IntegratedRootAttemptOutcome {
                attempt_op,
                pending: false,
                verification: Some(verification),
            });
        }
        self.begin_integrated_root_attempt(
            handle,
            root,
            changed,
            criteria,
            cancel,
            attempt_op,
            &ws,
            &candidate_snapshot,
            &goal,
            &repo_files,
        )
        .await
    }

    /// First call of one root-verification attempt: derive the required
    /// checks over the candidate root, execute the cheap ones inline and
    /// persist the attempt with one Queued job per expensive check. An open
    /// job of a crashed EARLIER attempt is superseded once (typed Cancelled
    /// rows) before the fresh attempt commits.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn begin_integrated_root_attempt(
        &self,
        handle: &faktor_session::SessionHandle,
        root: &std::path::Path,
        changed: &[String],
        criteria: &[String],
        cancel: &CancellationToken,
        attempt_op: u64,
        ws: &faktor_fs::WorkspaceHandle,
        candidate_snapshot: &str,
        goal: &str,
        repo_files: &[String],
    ) -> Result<IntegratedRootAttemptOutcome, String> {
        let service = self.deps.verification.clone();
        let row = handle
            .row()
            .map_err(|e| format!("session row unresolvable: {e}"))?;
        let task_id = row.task_id;
        let revision = handle
            .task_revision(task_id)
            .map_err(|e| format!("task revision read: {e}"))?
            .raw();
        let profile = faktor_verify::derive::detect_project_profile(root, repo_files);
        let changed_paths: Vec<std::path::PathBuf> =
            changed.iter().map(std::path::PathBuf::from).collect();
        let specs = faktor_verify::derive::derive_checks(&profile, &changed_paths)
            .map_err(|e| format!("verification derivation refused: {e}"))?;
        if specs.is_empty() {
            return Err("no derived checks apply to the integrated change".into());
        }
        let checks: Vec<faktor_verify::Check> = specs.iter().map(legacy_mirror_of_spec).collect();
        let order: std::collections::HashMap<String, String> = checks
            .iter()
            .map(|c| (c.id.clone(), c.command.clone()))
            .collect();
        let base_ctx = faktor_verify::exec::VerificationContext {
            session_id: handle.id().raw(),
            task_id: task_id.raw(),
            operation_id: attempt_op,
            workspace_id: row.workspace_id.raw(),
            worktree_id: row.worktree_id.raw(),
            root: root.to_path_buf(),
            deadline: std::time::Instant::now(),
            cancellation: cancel.child(),
        };
        let mut results: Vec<(String, bool)> = Vec::new();
        let mut executed: Vec<CheckExecution> = Vec::new();
        let mut unavailable: Vec<(String, String)> = Vec::new();
        let mut ordered: Vec<faktor_session::VerificationAttemptCheck> = Vec::new();
        let mut job_inputs: Vec<faktor_session::VerificationJobInput> = Vec::new();
        for spec in specs.iter().filter(|s| s.required) {
            let command = order.get(&spec.id).cloned().unwrap_or_default();
            let background = matches!(
                service.budget_for(spec),
                BudgetDecision::RunAsTaskOwnedOperation
            );
            if background {
                ordered.push(faktor_session::VerificationAttemptCheck {
                    check_id: spec.id.clone(),
                    command: command.clone(),
                    inline: None,
                });
                job_inputs.push(faktor_session::VerificationJobInput {
                    check_id: spec.id.clone(),
                    kind: format!("{:?}", spec.kind).to_ascii_lowercase(),
                    command: command.clone(),
                    program: spec.program.to_string_lossy().into_owned(),
                    args: spec
                        .args
                        .iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect(),
                    spec_json: serde_json::to_string(spec)
                        .map_err(|e| format!("job spec serialization: {e}"))?,
                    budget_ms: service.policy().unit_max.as_millis() as u64,
                });
                continue;
            }
            let budget = match service.budget_for(spec) {
                BudgetDecision::RunInline(budget) => budget,
                BudgetDecision::RunAsTaskOwnedOperation => service.policy().unit_max,
            };
            if budget.is_zero() {
                unavailable.push((spec.id.clone(), command.clone()));
                ordered.push(faktor_session::VerificationAttemptCheck {
                    check_id: spec.id.clone(),
                    command,
                    inline: Some(faktor_session::VerificationInlineStatus::Unavailable),
                });
                continue;
            }
            let mut vctx = base_ctx.clone();
            vctx.deadline = std::time::Instant::now() + budget;
            let outcome = service.execute(spec, &vctx).await;
            let status = match outcome.status {
                CheckRunStatus::Passed => VerificationStatus::Passed,
                CheckRunStatus::Failed => VerificationStatus::Failed,
                CheckRunStatus::Unavailable => VerificationStatus::Unavailable,
            };
            let inline = match outcome.status {
                CheckRunStatus::Passed => faktor_session::VerificationInlineStatus::Passed,
                CheckRunStatus::Failed => faktor_session::VerificationInlineStatus::Failed,
                CheckRunStatus::Unavailable => {
                    faktor_session::VerificationInlineStatus::Unavailable
                }
            };
            order.get(&spec.id);
            ordered.push(faktor_session::VerificationAttemptCheck {
                check_id: spec.id.clone(),
                command: command.clone(),
                inline: Some(inline),
            });
            match outcome.status {
                CheckRunStatus::Passed => results.push((spec.id.clone(), true)),
                CheckRunStatus::Failed => results.push((spec.id.clone(), false)),
                CheckRunStatus::Unavailable => {
                    unavailable.push((spec.id.clone(), command.clone()));
                }
            }
            executed.push(CheckExecution {
                check: spec.id.clone(),
                program: spec.program.to_string_lossy().into_owned(),
                args: spec
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
                category: format!("{:?}", spec.category).to_ascii_lowercase(),
                required: spec.required,
                status,
                started_ms: outcome.started_ms,
                finished_ms: Some(outcome.finished_ms),
                exit: outcome.exit,
                summary: outcome.summary.clone(),
            });
        }
        let root_str = root.to_string_lossy().into_owned();
        if job_inputs.is_empty() {
            // Every required check ran inline: no durable attempt is needed
            // (there is nothing an executor could settle later).
            let review = independent_completion_review(
                self.deps.as_ref(),
                handle,
                ws,
                changed,
                goal,
                repo_files,
                cancel,
            )
            .await;
            let status = match faktor_verify::acceptance(&checks, &results) {
                faktor_verify::Acceptance::Pass => VerificationStatus::Passed,
                faktor_verify::Acceptance::Fail => VerificationStatus::Failed,
                faktor_verify::Acceptance::Pending => VerificationStatus::Unavailable,
            };
            let review_evidence = admit_review_evidence(review.as_ref());
            let criteria_rows = criterion_verdicts_from_attempt(
                criteria,
                &checks,
                &results,
                &unavailable,
                changed,
                ws,
                review_evidence.as_ref(),
                candidate_snapshot,
                goal,
            )
            .await;
            return Ok(IntegratedRootAttemptOutcome {
                attempt_op,
                pending: false,
                verification: Some(IntegratedRootVerification {
                    status,
                    checks: executed,
                    criteria: criteria_rows,
                    changed: changed.to_vec(),
                    summary: format!(
                        "integrated-root verification: {} required check(s) inline over {}",
                        ordered.len(),
                        root.display()
                    ),
                    review_model_identity: review.as_ref().and_then(review_model_identity_of),
                }),
            });
        }
        // Persist the attempt: ordered checks (inline outcomes + job
        // definitions) in ONE transaction. An open job of an EARLIER
        // crashed attempt is superseded first (typed Cancelled rows) — an
        // open job is never silently replaced.
        let begin = handle.begin_verification_attempt(
            task_id.raw(),
            revision,
            attempt_op,
            &root_str,
            changed,
            &ordered,
            &job_inputs,
        );
        if let Err(err) = begin {
            let current = handle
                .current_verification_attempt(task_id.raw())
                .map_err(|e| format!("verification attempt read: {e}"))?;
            let Some(current) = current else {
                return Err(format!("verification attempt begin refused: {err}"));
            };
            let note = format!("superseded by the root verification attempt {attempt_op}");
            // The retried begin below surfaces its own typed refusal if the
            // cancel lost jobs; record this supersede write (marker + audit).
            self.dw_note_cancel_verification_attempt(
                handle,
                task_id.raw(),
                current.op_id,
                &note,
                DW_SITE_BEGIN_ATTEMPT_CANCEL,
            );
            handle
                .begin_verification_attempt(
                    task_id.raw(),
                    revision,
                    attempt_op,
                    &root_str,
                    changed,
                    &ordered,
                    &job_inputs,
                )
                .map_err(|e| format!("verification attempt begin refused: {e}"))?;
        }
        // Every enqueued job is still open by construction: the run is
        // pending exactly until the executor (or a later settlement)
        // resolves this exact attempt.
        Ok(IntegratedRootAttemptOutcome {
            attempt_op,
            pending: true,
            verification: None,
        })
    }

    /// Build the integrated-root verdict from a TERMINAL attempt rebuild
    /// (the turn settlement's exact tail): acceptance over the required
    /// mirrors, criterion verdicts through their own bindings, execution
    /// proof rows from the durable job results.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn root_verification_from_rebuild(
        &self,
        root: &std::path::Path,
        criteria: &[String],
        changed: &[String],
        ws: &faktor_fs::WorkspaceHandle,
        candidate_snapshot: &str,
        goal: &str,
        review: Option<&serde_json::Value>,
        rebuild: AttemptRebuild,
    ) -> IntegratedRootVerification {
        let AttemptRebuild {
            mirrors,
            results,
            unavailable,
            executed,
        } = rebuild;
        let checks: Vec<CheckExecution> = executed
            .into_iter()
            .map(|e| CheckExecution {
                check: e.id,
                program: e.program,
                args: e.args,
                category: match e.kind {
                    faktor_verify::CheckKind::Test => "test",
                    faktor_verify::CheckKind::Lint => "lint",
                    faktor_verify::CheckKind::Compile => "compile",
                }
                .into(),
                required: true,
                status: if e.passed {
                    VerificationStatus::Passed
                } else {
                    VerificationStatus::Failed
                },
                started_ms: e.started_ms,
                finished_ms: Some(e.finished_ms),
                exit: e.exit,
                summary: e.summary,
            })
            .collect();
        let status = match faktor_verify::acceptance(&mirrors, &results) {
            faktor_verify::Acceptance::Pass => VerificationStatus::Passed,
            faktor_verify::Acceptance::Fail => VerificationStatus::Failed,
            faktor_verify::Acceptance::Pending => VerificationStatus::Unavailable,
        };
        // Audit item 3: a durable review row is re-ADMITTED here (its
        // provenance tag must name an evidence-authoring class). A row that
        // cannot be admitted contributes NOTHING — the reviewer-dependent
        // criteria stay unresolved (fail closed), never a trusted pass.
        let review = review.and_then(|v| match ReviewEvidence::from_durable(v.clone()) {
            Ok(evidence) => Some(evidence),
            Err(refusal) => {
                tracing::warn!(
                    "durable review evidence refused by output trust ({}); reviewer-dependent \
                     criteria stay unresolved",
                    refusal
                );
                None
            }
        });
        let criteria_rows = criterion_verdicts_from_attempt(
            criteria,
            &mirrors,
            &results,
            &unavailable,
            changed,
            ws,
            review.as_ref(),
            candidate_snapshot,
            goal,
        )
        .await;
        IntegratedRootVerification {
            status,
            checks,
            criteria: criteria_rows,
            changed: changed.to_vec(),
            summary: format!(
                "integrated-root verification: {} required check(s) from the settled durable attempt ({} passed, {} failed, {} unavailable); root {}",
                mirrors.len(),
                results.iter().filter(|(_, ok)| *ok).count(),
                results.iter().filter(|(_, ok)| !ok).count(),
                unavailable.len(),
                root.display()
            ),
            review_model_identity: review
                .as_ref()
                .and_then(|r| review_model_identity_of(r.as_value())),
        }
    }

    /// Verify an EMPTY aggregate change set through the independent reviewer
    /// (P0 no-op policy): with no change there are no derived checks, so the
    /// ONLY objective mechanism is a reviewer-proved "no changes were
    /// necessary" verdict tied to the candidate snapshot. The review-model
    /// call is FORCED here (an empty local scan proves nothing); every
    /// acceptance criterion is then evaluated through its own binding with
    /// the reviewer record — RequiredCheck rows cannot pass over the missing
    /// check outcomes, so only criteria the reviewer can actually certify
    /// (`IndependentReview`/`AggregateGoal`) may pass. No reviewer verdict is
    /// a typed `Err`: the caller refuses completion, never mints proof.
    pub(crate) async fn verify_no_op_root(
        &self,
        handle: &faktor_session::SessionHandle,
        ws: &faktor_fs::WorkspaceHandle,
        criteria: &[String],
        candidate_snapshot: &str,
        goal: &str,
        cancel: &CancellationToken,
    ) -> Result<IntegratedRootVerification, String> {
        if criteria.is_empty() {
            return Err(
                "a no-op root carries no acceptance criterion to prove; no reviewer proof is possible"
                    .into(),
            );
        }
        let criteria_bounded: Vec<String> = criteria
            .iter()
            .take(REVIEW_NO_OP_MAX_CRITERIA)
            .map(|c| truncate(c, 300))
            .collect();
        let package = serde_json::json!({
            "kind": "no_op",
            "candidate_snapshot": candidate_snapshot,
            "changed_files": [],
            "criteria": criteria_bounded,
        })
        .to_string();
        let outcome = run_independent_review_call(
            self.deps.as_ref(),
            handle,
            &package,
            criteria,
            None,
            cancel,
        )
        .await;
        // Audit item 3: the no-op review evidence is admitted through the
        // LIVE reviewer output (verification opinion). Capture it before
        // the verdict moves out of the outcome; without an admitted output
        // there is no evidence to evaluate — a typed refusal.
        let Some(output) = outcome.output().cloned() else {
            return Err(
                "the no-op reviewer verdict carried no admitted verification-opinion output".into(),
            );
        };
        let Some(verdict) = outcome.verdict else {
            return Err(format!(
                "no independent reviewer verdict for the no-op root: {}",
                outcome
                    .refused
                    .unwrap_or_else(|| "the review call produced no typed verdict".into())
            ));
        };
        if verdict.verdict != faktor_verify::review::ReviewVerdictKind::Clean {
            return Err(format!(
                "the independent reviewer refused the no-op root: {:?} with findings {:?}",
                verdict.verdict,
                verdict.findings.iter().take(4).collect::<Vec<_>>()
            ));
        }
        let raw = serde_json::json!({
            "verdict": "pass",
            "findings": [],
            "evidence": [format!("no-op:{candidate_snapshot}")],
        });
        let review = match ReviewEvidence::from_review_output(&output, raw) {
            Ok(evidence) => evidence,
            Err(refusal) => {
                return Err(format!(
                    "the no-op reviewer output was refused by the trust gate: {refusal}"
                ))
            }
        };
        let criteria_rows = criterion_verdicts_from_attempt(
            criteria,
            &[],
            &[],
            &[],
            &[],
            ws,
            Some(&review),
            candidate_snapshot,
            goal,
        )
        .await;
        let passed = criteria_rows.iter().filter(|c| c.passed).count();
        if criteria_rows.is_empty() || passed != criteria_rows.len() {
            return Err(format!(
                "the independent reviewer's no-op proof does not certify every acceptance criterion ({passed} of {} passed)",
                criteria_rows.len()
            ));
        }
        Ok(IntegratedRootVerification {
            status: VerificationStatus::Passed,
            checks: Vec::new(),
            criteria: criteria_rows,
            changed: Vec::new(),
            summary: format!(
                "no-op root certified by the independent reviewer ({} criterion proof(s), candidate {candidate_snapshot})",
                criteria.len()
            ),
            // The ACTUAL review call's routed pair (the no-op path always
            // runs the reviewer): the orchestrator's reviewer digest must
            // name the real reviewer, never the parent's configured pair.
            review_model_identity: Some(ReviewModelIdentity {
                provider: outcome.provider,
                model: outcome.model,
            }),
        })
    }

    /// The mid-flight verdict of an unsettled background attempt: no gate,
    /// no record — the durable rows (task_state Verifying, last-run
    /// pending) reflect exactly what is true (jobs open). Never a
    /// completion and never a blocker.
    pub(crate) fn verification_pending_verdict(
        &self,
        handle: &faktor_session::SessionHandle,
        changed: &[String],
        reason: &str,
    ) -> TurnEndVerdict {
        tracing::info!(
            "session {}: background verification still pending — {reason}",
            handle.id()
        );
        self.persist_gate_facts(
            handle,
            &CompletionGate::VerificationPending,
            VerificationStatus::Pending,
            &[],
            changed,
            None,
        );
        TurnEndVerdict {
            verification: Vec::new(),
            acceptance: None,
            review: None,
            completion: Some(CompletionGate::VerificationPending),
            criteria: None,
            proof: None,
        }
    }

    /// Durable rows for one classified turn end (audits 4/6/7): the
    /// `task_state`/`state` row (the gate's task state), the bounded
    /// `verification`/`last` summary {status, checks, changed} and — once
    /// per goal, never rewritten — the `criteria`/`0` acceptance-criteria
    /// row (the canonical join of the entries that also seed the typed task
    /// row). Memory facts are durable rows: compaction operates on the
    /// transcript and ledger only and can NEVER rewrite them. Best-effort:
    /// an unwritable row is logged by the store, never fails the turn.
    pub(crate) fn persist_gate_facts(
        &self,
        handle: &faktor_session::SessionHandle,
        completion: &CompletionGate,
        status: VerificationStatus,
        results: &[(String, bool)],
        changed: &[String],
        criteria: Option<&[String]>,
    ) {
        let state = serde_json::to_value(completion.task_state())
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "pending".into());
        // Best-effort by contract (the verdict callers are infallible): every
        // lost row is recorded (marker + audit) and replayed on next open.
        self.dw_note_upsert_memory_fact(
            handle,
            FactSource::Durable,
            "task_state",
            "state",
            &state,
            DW_SITE_GATE_TASK_STATE_FACT,
        );
        let last = serde_json::json!({
            "status": serde_json::to_value(status).unwrap_or_else(|_| "pending".into()),
            "checks": results.iter().map(|(id, passed)| serde_json::json!({
                "id": truncate(id, 128),
                "passed": passed,
            })).collect::<Vec<_>>(),
            "changed": changed.iter().take(16).map(|p| truncate(p, 200)).collect::<Vec<_>>(),
        });
        let last = truncate(&serde_json::to_string(&last).unwrap_or_default(), 4000);
        self.dw_note_upsert_memory_fact(
            handle,
            FactSource::Durable,
            "verification",
            "last",
            &last,
            DW_SITE_GATE_LAST_FACT,
        );
        if let Some(criteria) = criteria {
            let text = criteria_canonical_text(criteria);
            let seeded = handle
                .memory_facts()
                .map(|facts| {
                    facts
                        .iter()
                        .any(|(kind, key, _)| kind == "criteria" && key == "0")
                })
                .unwrap_or(true);
            if !seeded {
                self.dw_note_upsert_memory_fact(
                    handle,
                    FactSource::Durable,
                    "criteria",
                    "0",
                    &text,
                    DW_SITE_GATE_CRITERIA_FACT,
                );
            }
        }
        // P0-79 site a: the verification/criteria fact rows were written at
        // a genuine end — the criteria status set changed (or was freshly
        // re-confirmed): semantic progress evidence.
        self.progress_gate_facts(handle.id(), results);
    }

    /// Open the session's EFFECTIVE workspace for the fingerprint's manifest
    /// probes — the same shadow-aware root verification executes against.
    /// `None` (no durable root / unopenable workspace) degrades the
    /// fingerprint to the honest "no manifest observation" state; this
    /// auxiliary evidence never blocks a durable record.
    pub(crate) fn fingerprint_workspace(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Option<faktor_fs::WorkspaceHandle> {
        let row = handle.row().ok()?;
        let root = self
            .deps
            .session
            .resolve_workspace_root(handle.id())
            .ok()??;
        self.deps.workspaces.open(row.workspace_id, root).ok()
    }

    /// The fingerprint's infallible half: everything observed from process
    /// metadata, the workspace filesystem and the durable task/instruction
    /// rows. Used by record construction (which adds the candidate reference)
    /// and by the background-attempt enqueue (which only needs the
    /// environment).
    pub(crate) fn observe_environment_fingerprint(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        check_basis: &[(String, String, Vec<String>)],
        workspace: Option<&faktor_fs::WorkspaceHandle>,
    ) -> (
        EnvironmentFingerprint,
        Vec<FingerprintFileHash>,
        Vec<FingerprintFileHash>,
    ) {
        let (manifest_hashes, lockfile_hashes) = fingerprint_build_inputs(workspace);
        let environment = EnvironmentFingerprint {
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            toolchain_versions: fingerprint_tool_versions(),
            manifest_hashes: manifest_hashes.clone(),
            lockfile_hashes: lockfile_hashes.clone(),
            instruction_epoch: self
                .session_instruction_epoch(handle)
                .map(|(epoch, _)| epoch),
            base_tree_hash: None, // documented honest unknown (see above)
            task_contract_hash: fingerprint_task_contract(handle, task_id),
            check_argv_cwd_env_hash: fingerprint_check_basis(check_basis),
            verification_impl_version: VERIFICATION_IMPL_VERSION.to_string(),
            proof_basis_digest: None,
        };
        (environment, manifest_hashes, lockfile_hashes)
    }

    /// Build the bounded environment fingerprint and the compact
    /// candidate-proof reference of ONE verification attempt (audits
    /// 94/116/117). What is knowable without subprocesses or the network:
    /// - `platform`/`arch`: the verifying process constants;
    /// - `toolchain_versions`: compile-time build metadata plus the
    ///   documented `RUSTUP_TOOLCHAIN` process variable when present. rustc/
    ///   cargo versions are NOT probed (a probe would be a subprocess); an
    ///   absent toolchain is omitted, never guessed;
    /// - `manifest_hashes`/`lockfile_hashes`: bounded whole-file BLAKE3
    ///   hashes of the known manifest/lockfile names present at the
    ///   verification root, streamed through the workspace handle;
    /// - `instruction_epoch`: the existing resolver epoch (None without a
    ///   durable root);
    /// - `base_tree_hash`: always `None` at this site — no whole-tree
    ///   observation is derivable without VCS/subprocess, and an honest
    ///   unknown beats a guessed digest;
    /// - `task_contract_hash`: BLAKE3 over the typed V2 criteria JSON of the
    ///   task row (the task contract);
    /// - `check_argv_cwd_env_hash`: BLAKE3 over the ordered check
    ///   (id, program, argv) basis + the root-relative cwd + a FIXED
    ///   allowlist of verification-relevant environment values (never the
    ///   full environment: secrets stay out of the digest);
    /// - `verification_impl_version`: this build's agent version.
    ///
    /// The candidate reference's accounting digest is the only fallible read
    /// (the durable budget ledger); every other part is deterministic for
    /// identical inputs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn verification_fingerprint(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        check_basis: &[(String, String, Vec<String>)],
        changed_files: &[FileStateEvidence],
        review: Option<&serde_json::Value>,
        workspace: Option<&faktor_fs::WorkspaceHandle>,
    ) -> faktor_core::Result<(EnvironmentFingerprint, CandidateProofRef)> {
        let (mut environment, manifest_hashes, lockfile_hashes) =
            self.observe_environment_fingerprint(handle, task_id, check_basis, workspace);
        // Authority law (audit P0/P1): the instruction epoch is part of the
        // REUSABLE proof basis. An unreadable workspace instruction root
        // refuses the basis — the projection's best-effort `None` must never
        // make two different instruction states alias the same key.
        environment.instruction_epoch = self
            .session_instruction_epoch_strict(handle)
            .map_err(|e| {
                faktor_core::Error::new(
                    faktor_core::ErrorKind::Conflict,
                    format!(
                        "workspace instruction root is unavailable; refusing to derive a reusable proof basis: {e}"
                    ),
                )
            })?
            .map(|(epoch, _)| epoch);
        // Candidate aggregates: `base` is the observed manifest/lockfile set
        // MINUS the paths this candidate changed (the build-input baseline);
        // `candidate` is the full observed set.
        let changed: std::collections::HashSet<&str> =
            changed_files.iter().map(|f| f.path.as_str()).collect();
        let entries: Vec<FingerprintFileHash> = manifest_hashes
            .iter()
            .chain(lockfile_hashes.iter())
            .cloned()
            .collect();
        let base_entries: Vec<FingerprintFileHash> = entries
            .iter()
            .filter(|e| !changed.contains(e.path.as_str()))
            .cloned()
            .collect();
        let source_diff_evidence = if changed_files.is_empty() {
            None
        } else {
            let mut rows: Vec<(&str, &str)> = changed_files
                .iter()
                .map(|f| (f.path.as_str(), f.digest_hex.as_str()))
                .collect();
            rows.sort();
            let parts: Vec<&[u8]> = rows
                .iter()
                .flat_map(|(path, digest)| [path.as_bytes(), digest.as_bytes()])
                .collect();
            Some(evidence_fold_hash(&parts))
        };
        let risk_report_evidence = match review {
            Some(value) => serde_json::to_vec(value)
                .ok()
                .map(|bytes| evidence_fold_hash(&[bytes.as_slice()])),
            None => None,
        };
        let candidate_proof_ref = CandidateProofRef {
            // Overwritten by the caller with the authoritative record
            // revision; the session write re-validates the match.
            task_revision: handle.task_revision(task_id)?,
            base_manifest_hash: fingerprint_file_aggregate(&base_entries),
            candidate_manifest_hash: fingerprint_file_aggregate(&entries),
            source_diff_evidence,
            risk_report_evidence,
            accounting_snapshot_digest: handle.accounting_snapshot_digest(task_id)?,
            run_id: None,
            run_base_snapshot: None,
            candidate_snapshot: None,
            sources_digest: None,
            changed_files_digest: None,
        };
        // P0 proof binding: one canonical proof-basis digest rides the
        // environment fingerprint; a record may be reused only while every
        // component below stays identical (task id+revision, task contract,
        // candidate snapshot, integration sources, changed files, ordered
        // checks, verifier/tool versions, env projection, instruction epoch,
        // criteria ids+bindings, reviewer/evidence digests).
        // P2-VERIFY: these reads feed the proof-basis digest that authorizes
        // REUSE. A store failure must refuse the basis (never silently fold
        // in an empty integration/criteria set and then reuse a weaker key).
        let integration = handle.ledger_integration_record_for_task(task_id.raw())?;
        // Identity law (audit P1): the candidate snapshot is part of a
        // REUSABLE proof basis. An unprovable content identity refuses the
        // basis — a synthetic marker must never key proof reuse.
        let candidate_snapshot = proof_basis_candidate_snapshot(workspace)?;
        let changed_files_digest = changed_files_fold(changed_files);
        let criteria: Vec<ProofBasisCriterion> = handle
            .get_task(task_id)?
            .map(|task| task.criteria())
            .unwrap_or_default()
            .into_iter()
            .map(|criterion| ProofBasisCriterion {
                criterion_id: criterion.id.to_string(),
                binding_digest: criterion.binding.as_ref().map(|b| b.content_digest()),
            })
            .collect();
        let env_projection: Vec<(String, String)> = FINGERPRINT_ENV_KEYS
            .iter()
            .map(|key| {
                let value = match std::env::var(key) {
                    Ok(value) if !value.is_empty() => value,
                    _ => "<absent>".to_string(),
                };
                ((*key).to_string(), value)
            })
            .collect();
        let reviewer_digest = review.and_then(|value| {
            serde_json::to_vec(value)
                .ok()
                .map(|bytes| format!("blake3:{}", blake3::hash(&bytes).to_hex()))
        });
        let basis = ProofBasis {
            task_id: task_id.raw(),
            task_revision: handle.task_revision(task_id)?.raw(),
            task_contract_digest: environment.task_contract_hash.clone(),
            candidate_snapshot: candidate_snapshot.clone(),
            integration_sources_digest: integration
                .as_ref()
                .map(|r| r.sources_digest.clone())
                .unwrap_or_default(),
            changed_files_digest: changed_files_digest.clone(),
            checks: check_basis
                .iter()
                .map(|(id, program, args)| ProofBasisCheck {
                    check_id: id.clone(),
                    program: program.clone(),
                    args: args.clone(),
                })
                .collect(),
            verification_impl_version: environment.verification_impl_version.clone(),
            tool_versions: environment.toolchain_versions.clone(),
            env_projection,
            instruction_epoch: environment.instruction_epoch,
            criteria,
            reviewer_digest,
            evidence_digests: {
                let mut digests: Vec<String> = changed_files
                    .iter()
                    .map(|f| format!("{}:{}", f.path, f.digest_hex))
                    .collect();
                digests.sort();
                digests
            },
        };
        environment.proof_basis_digest = Some(basis.digest());
        // Candidate provenance (P0): populated from the durable integration
        // record of an orchestrated root, so both IDEs can show the verified
        // snapshot, its run base, the source/change-set digests and the
        // landed candidate snapshot.
        let candidate_proof_ref = CandidateProofRef {
            run_id: integration.as_ref().map(|r| r.run_id.clone()),
            run_base_snapshot: integration.as_ref().and_then(|r| r.base_snapshot.clone()),
            candidate_snapshot: Some(candidate_snapshot),
            sources_digest: integration
                .as_ref()
                .map(|r| r.sources_digest.clone())
                .filter(|digest| !digest.is_empty()),
            changed_files_digest: Some(changed_files_digest),
            ..candidate_proof_ref
        };
        Ok((environment, candidate_proof_ref))
    }

    /// Strict-quality durable-criteria verification (audit 92): compare the
    /// `criteria`/`0` memory fact (the compaction-proof wave-9 canonical
    /// row, seeded once from the first derivation) against the typed task
    /// row's acceptance criteria at every genuine turn end. Both are seeded
    /// from the SAME canonical text ([`criteria_canonical_text`]), so any
    /// disagreement is crash residue or a hostile write — in Strict quality
    /// the completion claim is refused with a machine
    /// [`ReasonCode::CriteriaInconsistent`] reason. Deterministic: the next
    /// turn that derives criteria re-seeds the row from the fact's
    /// derivation, so a converged run re-verifies clean.
    ///
    /// Returns `Ok(Some(gate))` only when the caller's gate must be
    /// downgraded (the turn carries a completion claim). A non-mutating
    /// turn (no claim at stake) heals nothing and only warns — the row is
    /// re-seeded by the next mutating derivation.
    pub(crate) fn enforce_criteria_consistency(
        &self,
        handle: &faktor_session::SessionHandle,
        ledger: &TaskLedger,
        gate: Option<CompletionGate>,
    ) -> faktor_core::Result<Option<CompletionGate>> {
        let _ = ledger;
        // P2-VERIFY: both reads gate a completion claim; a store failure is
        // a typed refusal, never "the row was not seeded yet".
        let fact = handle
            .memory_facts()?
            .into_iter()
            .find(|(k, key, _)| k == "criteria" && key == "0")
            .map(|(_, _, v)| v);
        let row = self.session_task(handle)?;
        let row_criteria = row.map(|t| t.acceptance_criteria).unwrap_or_default();
        let (Some(fact), false) = (fact, row_criteria.is_empty()) else {
            // One side not yet seeded is the ordinary pre-derivation state
            // (or the crash window a same-turn sync already healed); only a
            // disagreement between two EXISTING rows is actionable.
            return Ok(None);
        };
        // Semantic agreement: the fact and the typed row agree when their
        // decoded criterion TEXTS agree. The representation may legitimately
        // move (legacy plain text -> typed V2 JSON on migration) without
        // being a divergence; a tampered criterion text still refuses.
        if criteria_texts_agree(&fact, &criteria_canonical_text(&row_criteria)) {
            return Ok(None);
        }
        let detail = format!(
            "durable criteria rows disagree: memory fact criteria/0 is {:?}; the typed task row carries {:?} — refusing the completion claim",
            truncate(&fact, 300),
            truncate(&criteria_canonical_text(&row_criteria), 300),
        );
        if gate.is_none() {
            tracing::warn!(
                session = %handle.id(),
                "durable criteria divergence without a completion claim; the next mutating derivation converges the row: {detail}"
            );
            return Ok(None);
        }
        tracing::error!(session = %handle.id(), "{detail}");
        let criteria_reason = OutcomeReason::new(ReasonCode::CriteriaInconsistent, detail);
        // Keep the gate kind the checks produced and ADD the divergence
        // reason: a machine must see the check verdict AND the row
        // corruption — never one silently overwriting the other.
        match gate {
            Some(CompletionGate::VerifiedComplete) | Some(CompletionGate::Unverified) => {
                Ok(Some(CompletionGate::BlockedVerification {
                    reasons: vec![criteria_reason],
                }))
            }
            Some(CompletionGate::VerificationPending) => {
                // Jobs are open and certify nothing yet: the pending gate
                // stands (the settlement turn re-checks the divergence when
                // the real gate lands — a completion can never certify over
                // inconsistent durable criteria).
                Ok(Some(CompletionGate::VerificationPending))
            }
            Some(CompletionGate::BlockedVerification { mut reasons }) => {
                reasons.push(criteria_reason);
                Ok(Some(CompletionGate::BlockedVerification { reasons }))
            }
            Some(CompletionGate::FailedVerification { mut reasons }) => {
                reasons.push(criteria_reason);
                Ok(Some(CompletionGate::FailedVerification { reasons }))
            }
            None => Ok(None),
        }
    }
}

/// The candidate content identity of a REUSABLE proof basis (audit P1
/// identity law): an absent workspace or an unprovable tree is a typed
/// refusal — `snapshot-unavailable` is a status value and must never key
/// proof reuse.
fn proof_basis_candidate_snapshot(
    workspace: Option<&faktor_fs::WorkspaceHandle>,
) -> faktor_core::Result<String> {
    let Some(workspace) = workspace else {
        return Err(faktor_core::Error::new(
            faktor_core::ErrorKind::Conflict,
            "the candidate workspace is unavailable; refusing to derive a reusable proof basis",
        ));
    };
    root_snapshot_best_effort(workspace).ok_or_else(|| {
        faktor_core::Error::new(
            faktor_core::ErrorKind::Conflict,
            "the candidate workspace has no provable content identity (special/oversized tree); \
             refusing to derive a reusable proof basis",
        )
    })
}
