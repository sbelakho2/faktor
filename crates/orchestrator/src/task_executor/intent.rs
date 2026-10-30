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
//! [`CLASSIFY_WINDOW_BYTES`] UTF-8-safe ASCII-lowercased bytes, politeness
//! prologue stripped once)
//!
//! Verdicts come from three verb/pattern CLASSES with explicit precedence:
//!
//! | # | Signal                                             | Verdict                                            |
//! |---|----------------------------------------------------|----------------------------------------------------|
//! | 1 | The request carries contract/verification fields   | [`TurnIntent::EstablishContract`]                 |
//! |   | (acceptance criteria, a criteria patch, or a       | (the user supplied the engineering seams directly) |
//! |   | completion contract)                               |                                                    |
//! | 2 | Leading CONVERSATIONAL pattern: an explanation /   | [`TurnIntent::Conversational`]                    |
//! |   | teaching / comparison / question lead ("explain",  | (class (a) WINS FIRST — "test my understanding",   |
//! |   | "summarize", "tell me", "walk/run/talk me through",| "run me through", "write an explanation" and      |
//! |   | "test/check my understanding", "write an           | "build a mental model" are chat even though "run", |
//! |   | explanation", "build a mental model", "compare",   | "test", "write" and "build" are ambiguous verbs)   |
//! |   | "contrast", "describe", "what/who/when/where/     |                                                    |
//! |   | which/why/how leads, ...)                          |                                                    |
//! | 3 | Leading explicit start-task escalation phrase      | [`TurnIntent::EstablishContract`]                 |
//! |   | ("start task", "begin the task", "start working    |                                                    |
//! |   | on", ...)                                          |                                                    |
//! | 4 | Leading STRONG engineering verb (class (b)):       | [`TurnIntent::EstablishContract`]                 |
//! |   | "fix", "implement", "patch", "refactor", "rename", | (a strong imperative engineering lead establishes  |
//! |   | "delete", "remove", "migrate", "upgrade",          | immediately)                                       |
//! |   | "replace", "modify", "edit", ...                   |                                                    |
//! | 5 | Leading AMBIGUOUS verb (class (c): "run", "test",  | [`TurnIntent::EstablishContract`]                 |
//! |   | "write", "build", "create", "make", "set", "add",  | (an ambiguous verb establishes ONLY with an        |
//! |   | "extend", "rework") WITH an engineering object in  | engineering object/pattern: "run tests", "run      |
//! |   | the same window (a path token, a source-code       | cargo", "write src/…", "create a PR", "set the     |
//! |   | suffix, or a bounded engineering-noun vocabulary)  | timeout to N"; without one it stays chat)          |
//! | 6 | Leading continuation phrase ("continue", "go on",  | [`TurnIntent::ContinueObjective`]                 |
//! |   | "keep going", "proceed", "carry on") OR a short    | (resolved against the ACTIVE objective;            |
//! |   | leading pronoun follow-up ("it", "that", "this",   | without one it degrades per the resolver: a        |
//! |   | "they", "those", "these")                          | mutation verb anywhere => Establish, else          |
//! |   |                                                    | Conversational — never a new objective)            |
//! | 7 | Everything else ("What is a mutex?", greetings,    | [`TurnIntent::Conversational`]                    |
//! |   | questions, descriptions)                           | (stays conversational end-to-end)                  |
//! |
//!
//! Verb-class membership (the audit's three-category table):
//!
//! - CONVERSATIONAL patterns (class (a), rule 2 — win FIRST): [`CONVERSATIONAL_PATTERNS`].
//! - STRONG engineering verbs (class (b), rule 4 — establish immediately): [`STRONG_VERBS`].
//! - AMBIGUOUS verbs (class (c), rule 5 — need an engineering object):
//!   [`AMBIGUOUS_VERBS`] + [`ENGINEERING_OBJECT_TOKENS`] + path/source-suffix
//!   detection. Object-pattern required forms the audit listed: "make X",
//!   "set the timeout to N", "edit the config", "modify the parser",
//!   "rework this API".
//!
//! Matching is deterministic ASCII-lowercased keyword logic with a light,
//! deterministic inflection fold ("fixing"→"fix", "tests"→"test",
//! "created"→"create") — never a model, never locale-sensitive lowercasing.
//!
//! # Honest limitations
//!
//! - **Non-English prompts**: the vocabulary is English/ASCII by design.
//!   A non-English imperative ("répare le parser") never matches a strong
//!   verb lead and therefore degrades to Conversational (rule 7) — a safe,
//!   deterministic false NEGATIVE. It never panics and never smuggles a
//!   contract: the false-positive posture stays asymmetric (questions and
//!   non-English prose cannot establish; the user can always attach
//!   explicit contract fields — rule 1 — or use a task run).
//! - **No LLM fallback in this module**: promoting a conversational verdict
//!   mid-turn would require a typed promotion request surfacing from the
//!   already-running conversational drive and a second admission inside
//!   the SAME turn loop (settlement + runtime seams, see the module
//!   report). That seam is deliberately NOT implemented here; the
//!   deterministic classifier stays the single admission-time authority.
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
//! Task identity (P2, audit): a materially NEW [`TurnIntent::EstablishContract`]
//! request never re-goals an unrelated ACTIVE objective on the same
//! identity — [`establishing_ask_mints_new_identity`] tells the executor to
//! advance the session to a FRESH `TaskId` (history/verification/evidence/
//! cost attribution stay separated per objective); a modification of the
//! CURRENT objective ([`materially_same_objective`]) and every continuation
//! keep the same identity/revision.
//!
//! Persistence: the resolved verdict rides its OWN op-keyed durable row
//! (`faktor_protocol::TURN_INTENT_ROW_KIND`, [`faktor_protocol::TurnIntentRow`])
//! — SEPARATE from any task row/ledger, so the chosen intent is durable
//! history that can never contaminate an engineering projection.

use std::collections::BTreeSet;

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

/// Class (a) — leading CONVERSATIONAL patterns (rule 2). These win BEFORE
/// every engineering verb class: "test my understanding", "run me through",
/// "write an explanation" and "build a mental model" are chat even though
/// "test", "run", "write" and "build" are ambiguous verbs. Word-boundary
/// prefix match against the prologue-stripped first line.
const CONVERSATIONAL_PATTERNS: &[&str] = &[
    // explanation / teaching
    "explain",
    "describe",
    "summarize",
    "summarise",
    "sum up",
    "recap",
    "define",
    "tell me",
    "tell us",
    "walk me through",
    "walk us through",
    "run me through",
    "run us through",
    "talk me through",
    "talk us through",
    "test my understanding",
    "test our understanding",
    "check my understanding",
    "check our understanding",
    "quiz me",
    "build a mental model",
    "build me a mental model",
    "give me an overview",
    "give me an explanation",
    "write an explanation",
    "write me an explanation",
    "write a summary",
    // comparison
    "compare",
    "contrast",
    "differentiate",
    // question leads (a question word at the head can never be an
    // imperative engineering lead)
    "what",
    "who",
    "whose",
    "when",
    "where",
    "which",
    "why",
    "how",
];

/// Leading explicit start-task escalation phrases (rule 3). Word-boundary
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

/// Class (b) — STRONG engineering verbs (rule 4): a leading strong verb
/// establishes IMMEDIATELY, no object pattern required. Singular tokens.
const STRONG_VERBS: &[&str] = &[
    "adjust",
    "apply",
    "change",
    "clean",
    "configure",
    "correct",
    "debug",
    "delete",
    "disable",
    "edit",
    "enable",
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
    "modify",
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
    "split",
    "tune",
    "upgrade",
    "update",
];

/// Class (c) — AMBIGUOUS verbs (rule 5): "run", "test", "write", "build",
/// "create", "make", plus the audit's missing mutation forms "set the
/// timeout to N", "add to/extend (code-objected)", "rework this API".
/// A leading ambiguous verb establishes ONLY with an engineering
/// object/pattern in the same window ([`has_engineering_object`]).
const AMBIGUOUS_VERBS: &[&str] = &[
    "add", "build", "create", "extend", "make", "rework", "run", "set", "test", "write",
];

/// The engineering-object vocabulary of the ambiguous-verb rule (rule 5).
/// Matched as whole tokens AFTER the leading verb; the leading verb itself
/// never satisfies its own object requirement ("build" alone is chat).
const ENGINEERING_OBJECT_TOKENS: &[&str] = &[
    "api",
    "benchmark",
    "benchmarks",
    "branch",
    "branches",
    "build",
    "builds",
    "cargo",
    "ci",
    "class",
    "classes",
    "client",
    "clients",
    "commit",
    "commits",
    "compiler",
    "config",
    "configs",
    "configuration",
    "dependencies",
    "dependency",
    "deployment",
    "deployments",
    "endpoint",
    "endpoints",
    "flag",
    "flags",
    "flaky",
    "function",
    "functions",
    "handler",
    "handlers",
    "indexer",
    "indexers",
    "linter",
    "lint",
    "lockfile",
    "migration",
    "migrations",
    "module",
    "modules",
    "package",
    "packages",
    "parser",
    "parsers",
    "pipeline",
    "pipelines",
    "pr",
    "prs",
    "query",
    "queries",
    "repo",
    "repositories",
    "repository",
    "router",
    "routers",
    "schema",
    "schemas",
    "server",
    "servers",
    "service",
    "services",
    "test",
    "tests",
    "timeout",
    "timeouts",
    "workflow",
    "workflows",
];

/// Source-code file suffixes that count as an engineering object ("write
/// src/…" is code-objected; a bare word never matches — the dot is part of
/// the token).
const SOURCE_SUFFIXES: &[&str] = &[
    ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".kt", ".kts", ".toml", ".json",
    ".yaml", ".yml", ".sql", ".sh", ".bash", ".css", ".html", ".c", ".h", ".cpp", ".hpp", ".rb",
    ".php", ".swift",
];

/// Leading continuation phrases (rule 6, first half).
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

/// Leading pronoun follow-ups (rule 6, second half): a short pronoun-led
/// message resolves against the ACTIVE objective when one exists; it can
/// never create a new one.
const PRONOUN_FOLLOWUPS: &[&str] = &["it", "that", "this", "they", "those", "these"];

/// Function words excluded from the goal-relatedness content tokens (P2).
/// Verb tokens are additionally excluded via the verb-class sets.
const GOAL_STOPWORDS: &[&str] = &[
    "about", "after", "again", "all", "also", "an", "and", "any", "are", "as", "at", "be", "been",
    "but", "by", "can", "could", "did", "do", "does", "done", "down", "even", "every", "from",
    "get", "had", "has", "have", "he", "her", "here", "hers", "him", "his", "how", "i", "if", "in",
    "into", "is", "it", "its", "just", "like", "may", "me", "might", "mine", "more", "most",
    "must", "my", "no", "not", "of", "off", "on", "once", "only", "or", "other", "our", "out",
    "over", "please", "same", "she", "should", "so", "some", "such", "than", "that", "the",
    "their", "them", "then", "there", "these", "they", "this", "those", "through", "to", "too",
    "under", "up", "us", "very", "was", "we", "were", "what", "when", "where", "which", "who",
    "why", "will", "with", "would", "you", "your",
];

/// The shared byte-bounded UTF-8-safe prefix primitive: the longest prefix
/// of `s` of at most `max_bytes` bytes that never splits a multi-byte char
/// (backs off to a char boundary). EVERY byte-sliced user-controlled UTF-8
/// string in this module goes through here — `&s[..n]` panics when byte
/// `n` splits a multi-byte character (e.g. 511 ASCII bytes + "é").
/// Total for any valid UTF-8 input.
pub fn utf8_prefix(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

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
/// `prompt` the classifier inspects. Never allocates beyond the window, and
/// the byte bound is UTF-8-safe ([`utf8_prefix`]) — the classifier is TOTAL
/// for any valid UTF-8.
fn classify_window(prompt: &str) -> String {
    let slice = utf8_prefix(prompt, CLASSIFY_WINDOW_BYTES);
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

/// De-double a final doubled consonant ("runn"→"run", "sett"→"set");
/// "s"/"z" doubling is morphological ("class", "buzz") and never folded.
fn de_double(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let last = *b.last()?;
    if b.len() >= 3 && b[b.len() - 1] == b[b.len() - 2] && !matches!(last, b's' | b'z') {
        Some(&s[..s.len() - 1])
    } else {
        None
    }
}

/// Deterministic light inflection fold: does `token` equal (or inflect to)
/// any member of `set`? Handles the obvious English forms only —
/// "fixing"→"fix", "fixed"→"fix", "tests"→"test", "created"→"create",
/// "running"→"run", "setting"→"set". ASCII-only, suffix-table driven, no
/// stemming library, no locale.
fn token_matches(token: &str, set: &[&str]) -> bool {
    if set.contains(&token) {
        return true;
    }
    let mut bases: Vec<&str> = Vec::with_capacity(5);
    if let Some(b) = token.strip_suffix("ing") {
        bases.push(b);
    }
    if let Some(b) = token.strip_suffix("ed") {
        bases.push(b);
    }
    if token.ends_with("ed") && token.len() > 3 {
        // "created" → "create"
        bases.push(&token[..token.len() - 1]);
    }
    if let Some(b) = token.strip_suffix("es") {
        bases.push(b);
    }
    if let Some(b) = token.strip_suffix("s") {
        bases.push(b);
    }
    for base in bases {
        if set.contains(&base) {
            return true;
        }
        if let Some(folded) = de_double(base) {
            if set.contains(&folded) {
                return true;
            }
        }
        // "making" → "make"
        let mut with_e = String::with_capacity(base.len() + 1);
        with_e.push_str(base);
        with_e.push('e');
        if set.contains(&with_e.as_str()) {
            return true;
        }
    }
    false
}

/// The leading-verb class of the prologue-stripped window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeadingVerb {
    None,
    /// Class (b): establishes immediately.
    Strong,
    /// Class (c): establishes only with an engineering object.
    Ambiguous,
}

fn leading_verb(lowered: &str) -> LeadingVerb {
    let Some(first) = lowered.split_whitespace().next() else {
        return LeadingVerb::None;
    };
    let first = first.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\'');
    if token_matches(first, STRONG_VERBS) {
        LeadingVerb::Strong
    } else if token_matches(first, AMBIGUOUS_VERBS) {
        LeadingVerb::Ambiguous
    } else {
        LeadingVerb::None
    }
}

/// The engineering-object rule of the ambiguous-verb class (rule 5): the
/// window (AFTER the leading verb) must carry a path token, a source-code
/// suffix, or a whole-token hit of [`ENGINEERING_OBJECT_TOKENS`].
/// "run tests", "run cargo", "run the build", "write src/…", "write a test
/// for", "build the endpoint", "create a PR", "set the timeout to N" →
/// object present; "run me through", "write an explanation", "test my
/// understanding", "build a mental model" → object absent (and the class
/// (a) patterns above already classify those conversational).
fn has_engineering_object(lowered: &str) -> bool {
    let mut tokens = lowered.split_whitespace();
    // The leading verb itself never satisfies its own object requirement.
    tokens.next();
    for token in tokens {
        if token.contains('/') || token.contains('\\') {
            return true;
        }
        if SOURCE_SUFFIXES.iter().any(|suffix| token.ends_with(suffix)) {
            return true;
        }
        let bare = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\'');
        if ENGINEERING_OBJECT_TOKENS.contains(&bare) {
            return true;
        }
    }
    false
}

/// True when a mutation verb appears ANYWHERE in the bounded window
/// (word-boundary matched, inflection-folded; strong + ambiguous classes).
/// Used only by the continuation fall-through.
fn contains_mutation_verb(window: &str) -> bool {
    window
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
        .any(|token| {
            let token = token.trim_matches('\'');
            token_matches(token, STRONG_VERBS) || token_matches(token, AMBIGUOUS_VERBS)
        })
}

/// The PURE classifier (rules 1-7 of the module table): request fields +
/// prompt bytes only. No session, no model, no I/O — cheap and total for
/// any valid UTF-8.
pub fn classify_turn_intent(goal: &str, carries_fields: bool) -> TurnIntent {
    if carries_fields {
        return TurnIntent::EstablishContract;
    }
    let window = classify_window(goal);
    if window.is_empty() {
        return TurnIntent::Conversational;
    }
    // Rule 2 (class (a)): conversational patterns win FIRST.
    if CONVERSATIONAL_PATTERNS
        .iter()
        .any(|pattern| leads_with(&window, pattern))
    {
        return TurnIntent::Conversational;
    }
    // Rule 3: explicit start-task escalation.
    if ESCALATION_PHRASES
        .iter()
        .any(|phrase| leads_with(&window, phrase))
    {
        return TurnIntent::EstablishContract;
    }
    // Rules 4/5 (classes (b)/(c)): strong verbs establish immediately,
    // ambiguous verbs establish only with an engineering object.
    match leading_verb(&window) {
        LeadingVerb::Strong => return TurnIntent::EstablishContract,
        LeadingVerb::Ambiguous if has_engineering_object(&window) => {
            return TurnIntent::EstablishContract
        }
        LeadingVerb::Ambiguous | LeadingVerb::None => {}
    }
    // Rule 6: continuation phrase or short pronoun follow-up.
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
    // Rule 7: everything else stays conversational.
    TurnIntent::Conversational
}

/// The content tokens of a recorded/new goal used by the P2
/// relatedness check: ASCII-lowercased, alphanumeric-split, function
/// words AND verb-class tokens excluded (the shared verb "fix" says
/// nothing about whether two objectives are the same).
fn goal_content_tokens(goal: &str) -> BTreeSet<String> {
    goal.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
        .map(|t| t.trim_matches('\''))
        .filter(|t| t.len() >= 2)
        .filter(|t| !GOAL_STOPWORDS.contains(t))
        .filter(|t| !token_matches(t, STRONG_VERBS) && !token_matches(t, AMBIGUOUS_VERBS))
        .map(str::to_owned)
        .collect()
}

/// P2 task identity: is `new_goal` a modification of the CURRENT objective
/// `current_goal` (same TaskId, revision bump) rather than a materially
/// NEW one (fresh TaskId)? Deterministic, bounded, keyword-based: equal
/// goals (case-insensitive) are the same objective; otherwise the two
/// content-token sets must share at least one non-noise token. An empty
/// side (vague "fix it more" against a goal, or an empty recorded goal)
/// cannot prove relatedness and conservatively keeps the identity.
///
/// Honest limitation: this is a cheap deterministic heuristic, not
/// semantics — two objectives about the same noun ("fix the login
/// redirect" vs "fix the logout redirect" share "redirect") stay on one
/// identity, and a re-scoped objective that shares no content token opens
/// a fresh one. History/verification/evidence/cost attribution stays
/// separated exactly at the token-overlap seam.
pub fn materially_same_objective(current_goal: &str, new_goal: &str) -> bool {
    if current_goal.trim().eq_ignore_ascii_case(new_goal.trim()) {
        return true;
    }
    let current = goal_content_tokens(current_goal);
    let fresh = goal_content_tokens(new_goal);
    if current.is_empty() || fresh.is_empty() {
        return true;
    }
    current.intersection(&fresh).next().is_some()
}

/// P2 task identity: does THIS ask admission mint a FRESH TaskId instead of
/// re-goaling the active non-terminal row in place? True only for a
/// promoted [`TurnIntent::EstablishContract`] whose goal is materially
/// unrelated to the current objective ([`materially_same_objective`]).
/// Continuations (`suppress_regal`) and legacy `task` starts (no ask
/// decision) never mint — they keep the historical same-identity re-goal.
pub fn establishing_ask_mints_new_identity(
    decision: Option<AskAdmissionDecision>,
    current_goal: &str,
    new_goal: &str,
) -> bool {
    match decision {
        Some(decision)
            if decision.intent == TurnIntent::EstablishContract
                && decision.promote
                && !decision.suppress_regal =>
        {
            !materially_same_objective(current_goal, new_goal)
        }
        _ => false,
    }
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
///   admission path (a fresh objective when the row is free; when a
///   non-terminal row exists, a MATERIALLY NEW objective mints a NEW
///   `TaskId` — [`establishing_ask_mints_new_identity`] — while an
///   explicit modification of the CURRENT task re-goals it in place on
///   the SAME identity/revision);
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
