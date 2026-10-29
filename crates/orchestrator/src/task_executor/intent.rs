//! Lazy contract establishment (audit item 4): the deterministic, typed
//! intent classifier of ONE ordinary chat turn.
//!
//! Faktor has NO user-selected Ask/Task mode: every message is an ordinary
//! chat turn, and an engineering contract/objective is LAZILY activated per
//! turn through the SAME executor admission path (same authority, no second
//! agent). This module is the ONE classifier both the executor admission
//! ([`super::TaskExecutor::start_task_with_turn_cap`]) and the server prompt
//! facade (`faktor-server`'s `PromptExecutionService`) consult; it is
//! CHEAP, DETERMINISTIC and NEVER calls a model.
//!
//! # Rule table (first match wins; bounded: first non-empty line, first
//! [`CLASSIFY_WINDOW_BYTES`] ASCII-lowercased bytes, politeness prologue
//! stripped once)
//!
//! | # | Signal                                             | Verdict                                            |
//! |---|----------------------------------------------------|----------------------------------------------------|
//! | 1 | The request carries contract/verification fields   | [`TurnIntent::EstablishContract`]                 |
//! |   | (acceptance criteria, a criteria patch, or a       | (the user supplied the engineering seams directly) |
//! |   | completion contract)                               |                                                    |
//! | 2 | Leading explicit start-task escalation phrase      | [`TurnIntent::EstablishContract`]                 |
//! |   | ("start task", "begin the task", "start working    |                                                    |
//! |   | on", ...)                                          |                                                    |
//! | 3 | Leading explicit mutation/implement verb after the | [`TurnIntent::EstablishContract`]                 |
//! |   | politeness prologue ("fix", "implement", "add",    | (an imperative engineering lead)                   |
//! |   | "refactor", "patch", ...)                          |                                                    |
//! | 4 | Leading continuation phrase ("continue", "go on",  | [`TurnIntent::ContinueObjective`]                 |
//! |   | "keep going", "proceed", "carry on") OR a short    | (resolved against the ACTIVE objective;            |
//! |   | leading pronoun follow-up ("it", "that", "this",   | without one it degrades per the resolver: a        |
//! |   | "they", "those", "these")                          | mutation verb anywhere => Establish, else          |
//! |   |                                                    | Conversational — never a new objective)            |
//! | 5 | Everything else ("What is a mutex?", greetings,    | [`TurnIntent::Conversational`]                    |
//! |   | questions, descriptions)                           | (stays conversational end-to-end)                  |
//! |
//!
//! Determinism contract: the same (request fields, prompt bytes, active
//! objective presence) always yields the same verdict — no clock, no model,
//! no I/O in the pure classifier ([`classify_turn_intent`]); the session
//! consult ([`resolve_ask_admission`]) reads ONLY the durable task row of
//! the SAME handle and is the documented single I/O point. False-positive
//! posture is deliberately asymmetric: a question or description can never
//! establish a contract (questions lead with question words, not verbs), a
//! conversational turn NEVER reads the engineering ledger, and a
//! continuation only ever REUSES the active objective — it can never mint a
//! new one.
//!
//! Persistence: the resolved verdict rides its OWN op-keyed durable row
//! (`faktor_protocol::TURN_INTENT_ROW_KIND`, [`faktor_protocol::TurnIntentRow`])
//! — SEPARATE from any task row/ledger, so the chosen intent is durable
//! history that can never contaminate an engineering projection.

use faktor_protocol::{ExecutionKind, TurnIntent};

use super::{ExecError, TaskRunRequest};
use crate::WorkKind;

/// Bound on the prompt bytes the classifier inspects (bounded everything: a
/// hostile 10 MiB prompt is classified from its first slice, never
/// scanned whole).
pub const CLASSIFY_WINDOW_BYTES: usize = 512;

/// Bound on the words the short pronoun follow-up rule inspects.
const PRONOUN_FOLLOWUP_MAX_WORDS: usize = 12;

/// The politeness prologue stripped once before the leading-signal rules
/// (a request "Please fix the tests" is the same imperative as "fix the
/// tests").
const POLITENESS_PROLOGUES: &[&str] = &[
    "please ",
    "pls ",
    "kindly ",
    "can you ",
    "could you ",
    "would you ",
    "will you ",
    "i want you to ",
    "i'd like you to ",
    "help me ",
    "help me to ",
    "let's ",
    "lets ",
];

/// Leading explicit start-task escalation phrases (rule 2). Word-boundary
/// prefix match against the prologue-stripped first line.
const ESCALATION_PHRASES: &[&str] = &[
    "start task",
    "start a task",
    "start the task",
    "begin task",
    "begin a task",
    "begin the task",
    "start implementation",
    "begin implementation",
    "start implementing",
    "begin implementing",
    "start working on",
    "begin working on",
    "kick off the task",
];

/// Leading explicit mutation/implement verbs (rule 3), singular tokens.
const MUTATION_VERBS: &[&str] = &[
    "add",
    "adjust",
    "apply",
    "build",
    "change",
    "clean",
    "configure",
    "correct",
    "create",
    "debug",
    "delete",
    "disable",
    "enable",
    "extend",
    "extract",
    "fix",
    "harden",
    "implement",
    "improve",
    "increase",
    "install",
    "inline",
    "migrate",
    "merge",
    "move",
    "optimize",
    "patch",
    "port",
    "reduce",
    "remove",
    "rename",
    "repair",
    "replace",
    "restore",
    "rewrite",
    "refactor",
    "run",
    "split",
    "test",
    "tune",
    "upgrade",
    "update",
    "write",
];

/// Leading continuation phrases (rule 4, first half).
const CONTINUATION_PHRASES: &[&str] = &[
    "continue",
    "continuing",
    "go on",
    "go ahead",
    "keep going",
    "proceed",
    "carry on",
    "resume",
    "pick up where",
];

/// Leading pronoun follow-ups (rule 4, second half): a short pronoun-led
/// message resolves against the ACTIVE objective when one exists; it can
/// never create a new one.
const PRONOUN_FOLLOWUPS: &[&str] = &["it", "that", "this", "they", "those", "these"];

/// Whether [`TaskRunRequest`] carries user-supplied contract/verification
/// fields (rule 1). Budget caps are NOT contract fields — they stay a typed
/// refusal on a conversational ask (they bind to a durable task row a
/// conversational turn never creates).
pub fn carries_contract_fields(req: &TaskRunRequest) -> bool {
    !req.criteria.is_empty()
        || req
            .criteria_patch
            .as_ref()
            .is_some_and(|patch| !patch.is_empty())
        || req.completion_contract.is_some()
}

/// The bounded, prologue-stripped, ASCII-lowercased first non-empty line of
/// `prompt` the classifier inspects. Never allocates beyond the window.
fn classify_window(prompt: &str) -> String {
    let slice = &prompt[..prompt.len().min(CLASSIFY_WINDOW_BYTES)];
    let line = slice
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut lowered = line.to_ascii_lowercase();
    loop {
        let stripped = POLITENESS_PROLOGUES
            .iter()
            .find_map(|prologue| lowered.strip_prefix(prologue))
            .map(str::to_owned);
        match stripped {
            Some(next) => lowered = next.trim_start().to_owned(),
            None => break,
        }
    }
    lowered
}

/// True when `lowered` starts with `phrase` on a word boundary.
fn leads_with(lowered: &str, phrase: &str) -> bool {
    match lowered.strip_prefix(phrase) {
        None => false,
        Some(rest) => {
            rest.is_empty()
                || rest.starts_with(' ')
                || rest.starts_with(|c: char| !c.is_ascii_alphanumeric())
        }
    }
}

/// True when a mutation verb appears ANYWHERE in the bounded window
/// (word-boundary matched). Used only by the continuation fall-through.
fn contains_mutation_verb(window: &str) -> bool {
    window
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
        .any(|token| MUTATION_VERBS.contains(&token))
}

/// The PURE classifier (rules 1-5 of the module table): request fields +
/// prompt bytes only. No session, no model, no I/O — cheap and total.
pub fn classify_turn_intent(goal: &str, carries_fields: bool) -> TurnIntent {
    if carries_fields {
        return TurnIntent::EstablishContract;
    }
    let window = classify_window(goal);
    if window.is_empty() {
        return TurnIntent::Conversational;
    }
    // Rule 2: explicit start-task escalation.
    if ESCALATION_PHRASES
        .iter()
        .any(|phrase| leads_with(&window, phrase))
    {
        return TurnIntent::EstablishContract;
    }
    // Rule 3: leading explicit mutation/implement verb.
    if let Some(first) = window.split_whitespace().next() {
        let first = first.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\'');
        if MUTATION_VERBS.contains(&first) {
            return TurnIntent::EstablishContract;
        }
    }
    // Rule 4: continuation phrase or short pronoun follow-up.
    if CONTINUATION_PHRASES
        .iter()
        .any(|phrase| leads_with(&window, phrase))
    {
        return TurnIntent::ContinueObjective;
    }
    let words = window.split_whitespace().count();
    if words <= PRONOUN_FOLLOWUP_MAX_WORDS {
        if let Some(first) = window.split_whitespace().next() {
            let first = first.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\'');
            if PRONOUN_FOLLOWUPS.contains(&first) {
                return TurnIntent::ContinueObjective;
            }
        }
    }
    // Rule 5: everything else stays conversational.
    TurnIntent::Conversational
}

/// The admission verdict of ONE ask-classified request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskAdmissionDecision {
    /// The resolved, durably-persisted intent.
    pub intent: TurnIntent,
    /// Run this request with ENGINEERING-task semantics (the existing
    /// executor admission path: durable task row, contract compile, shadow
    /// isolation, verification + completion gate). `false` keeps the
    /// conversational ask path byte-identical.
    pub promote: bool,
    /// A continuation that resolves against the ACTIVE objective: the
    /// admitted run must NOT re-goal the durable row (the objective is
    /// reused verbatim; "continue" is never recorded as a new goal).
    pub suppress_regal: bool,
}

/// Whether the session's CURRENT durable task row is an ACTIVE engineering
/// objective (non-terminal row with a non-empty goal). A store read
/// failure refuses typed — it never silently becomes "no objective".
fn active_objective(handle: &faktor_session::SessionHandle) -> Result<bool, ExecError> {
    let task_id = handle
        .task_id()
        .map_err(|e| ExecError::Internal(format!("task id unreadable: {e}")))?;
    let task = handle
        .get_task(task_id)
        .map_err(|e| ExecError::Internal(format!("task row unreadable: {e}")))?;
    Ok(task.is_some_and(|task| !task.state.is_terminal() && !task.goal.trim().is_empty()))
}

/// Resolve ONE ask-classified request against the session's active
/// objective (the ONLY I/O point of this module, see the module docs):
///
/// - [`TurnIntent::Conversational`] — the turn stays conversational
///   end-to-end (no promotion);
/// - [`TurnIntent::EstablishContract`] — promote to the engineering
///   admission path (a fresh objective when the row is free; the existing
///   task path's documented re-goal when a non-terminal row exists — an
///   UNRELATED request creates a new objective on the SAME identity);
/// - [`TurnIntent::ContinueObjective`] — promote WITHOUT re-goaling when
///   an active objective exists; WITHOUT one the follow-up degrades:
///   a mutation verb anywhere in the bounded window establishes a contract
///   ("continue fixing the flaky test"), anything else stays conversational
///   (a "continue" into the void is a chat turn, never a fabricated
///   objective).
pub fn resolve_ask_admission(
    handle: &faktor_session::SessionHandle,
    req: &TaskRunRequest,
) -> Result<AskAdmissionDecision, ExecError> {
    let verdict = classify_turn_intent(&req.goal, carries_contract_fields(req));
    match verdict {
        TurnIntent::Conversational => Ok(AskAdmissionDecision {
            intent: TurnIntent::Conversational,
            promote: false,
            suppress_regal: false,
        }),
        TurnIntent::EstablishContract => Ok(AskAdmissionDecision {
            intent: TurnIntent::EstablishContract,
            promote: true,
            suppress_regal: false,
        }),
        TurnIntent::ContinueObjective => {
            if active_objective(handle)? {
                Ok(AskAdmissionDecision {
                    intent: TurnIntent::ContinueObjective,
                    promote: true,
                    suppress_regal: true,
                })
            } else if contains_mutation_verb(&classify_window(&req.goal)) {
                Ok(AskAdmissionDecision {
                    intent: TurnIntent::EstablishContract,
                    promote: true,
                    suppress_regal: false,
                })
            } else {
                Ok(AskAdmissionDecision {
                    intent: TurnIntent::Conversational,
                    promote: false,
                    suppress_regal: false,
                })
            }
        }
    }
}

/// Apply a promoting verdict to the request BEFORE validation: the request
/// becomes an ordinary engineering run of the SAME executor (the work item
/// flips to the mutating Implementation kind so the isolation/contract
/// policy applies exactly as a legacy task start). Conversational requests
/// pass through untouched.
pub fn promote_ask_request(req: &mut TaskRunRequest, decision: &AskAdmissionDecision) {
    if !decision.promote {
        return;
    }
    req.execution = ExecutionKind::Task;
    if let Some(item) = req.work_items.first_mut() {
        item.kind = WorkKind::Implementation;
        // The item was CONSTRUCTED read-only (the conversational admission
        // shape carries NoWrites). A promoted MUTATING item takes exactly
        // the ownership a task start's mutating item gets by default
        // (`kind_default_ownership`): the daemon-owned isolated candidate.
        // An item that already carries write ownership (Paths /
        // IsolatedWorktree / SemanticEntities) keeps it — the promotion
        // never DOWNGRADES an explicit client choice.
        if matches!(item.ownership, faktor_core::state::OwnershipSpec::NoWrites) {
            item.ownership = faktor_core::state::OwnershipSpec::IsolatedWorktree;
        }
    }
}
