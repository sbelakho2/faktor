//! Hostile-corpus certification for the pure core types (no I/O).
//!
//! Every case is individually asserted with a message naming the category,
//! case number and the exact input. All exercises run through the production
//! types/constructors: nothing here re-implements a validator.
//!
//! Categories (see the task report for exact counts):
//! 1. agent/session state-machine full matrices;
//! 2. journal sequence/timestamp boundaries and the event vocabulary;
//! 3. identifier boundaries and serde;
//! 4. mime/filename/workspace-path validator corpora;
//! 5. ownership/change-budget/criterion-binding corpora;
//! 6. authority-digest domain separation and classification;
//! 7. file hashes, deadlines, op metadata;
//! 8. retry-policy matrix;
//! 9. resource gauges, capability sets, network policy;
//! 10. child-blocker/phase vocabularies;
//! 11. deterministic-LCG property laws.

use std::panic::catch_unwind;

use crate::attachment::{
    validate_filename, validate_mime, AttachmentId, AttachmentRef, MAX_ATTACHMENT_BYTES,
    MAX_ATTACHMENT_FILENAME_BYTES, MAX_ATTACHMENT_MIME_BYTES,
};
use crate::authority::{
    authority_digest_labeled, classify_authority_digest, refuse_legacy_authority_digest,
    AuthorityDigestKind, Fields, DOMAIN_ACCOUNTING_BALANCE, DOMAIN_BASE_MAP,
    DOMAIN_CANDIDATE_MANIFEST, DOMAIN_CHANGED_FILES, DOMAIN_CHANGE_SET, DOMAIN_CHECK_BASIS,
    DOMAIN_CHECK_EXECUTION, DOMAIN_COMMAND_BINDING, DOMAIN_CRITERION_BINDING,
    DOMAIN_INTEGRATION_SOURCES, DOMAIN_RUN_BASE_MANIFEST, DOMAIN_SEMANTIC_FACT,
    DOMAIN_TASK_CONTRACT, MAX_AUTHORITY_FIELD_BYTES,
};
use crate::blocker::{
    child_lifecycle_tag_is_known, validate_child_runtime_state, BlockerKind, ChildBlocker,
    ExecutionPhase, CHILD_LIFECYCLE_TAGS, MAX_CHILD_BLOCKER_DEPENDENCY_CHARS,
    MAX_CHILD_BLOCKER_REASON_CHARS,
};
use crate::capability::{CapabilityKind, CapabilitySet, NetworkPolicy};
use crate::error::ErrorKind;
use crate::event::{Event, EventKind, JournalInvariants};
use crate::hash::FileHash;
use crate::id::{
    EventSeq, OpId, ProviderCallId, SessionId, TaskId, TaskRevision, VerificationRecordId,
    WorkspaceId, WorktreeId,
};
use crate::op::{ModelCallAttempt, OpMeta, RecoveryStrategy};
use crate::path::NormalizedWorkspacePath;
use crate::resource::{ResourceClass, ResourceGauge, ResourceLimits};
use crate::retry::{RetryClass, RetryPolicy};
use crate::state::{
    canonical_command_text, command_binding_digest, command_binding_digest_parts,
    legacy_binding_for_criterion_text, AgentState, ChangeBudget, CriterionBinding, NoOpDisposition,
    OwnershipSpec, SessionLifecycle, StateMachine,
};
use crate::time::{Clock, Deadline, TestClock};

const AGENT_STATES: [AgentState; 17] = [
    AgentState::Idle,
    AgentState::Preparing,
    AgentState::BuildingContext,
    AgentState::WaitingForModel,
    AgentState::Streaming,
    AgentState::ToolRequested,
    AgentState::WaitingForPermission,
    AgentState::ExecutingTool,
    AgentState::Validating,
    AgentState::UpdatingMemory,
    AgentState::ReadyForNextTurn,
    AgentState::Completed,
    AgentState::Cancelled,
    AgentState::FailedRecoverable,
    AgentState::FailedPermanent,
    AgentState::NeedsUserInput,
    AgentState::Suspended,
];

const LIFECYCLES: [SessionLifecycle; 5] = [
    SessionLifecycle::Open,
    SessionLifecycle::Suspended,
    SessionLifecycle::Closing,
    SessionLifecycle::Closed,
    SessionLifecycle::FailedPermanent,
];

/// Deterministic 64-bit LCG (no external RNG; seeds are fixed in the test).
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
}

fn hash(byte: u8) -> FileHash {
    FileHash::from([byte; 32])
}

#[test]
fn core_agent_state_machine_full_matrix_is_individually_asserted() {
    let mut case = 0usize;
    for from in AGENT_STATES {
        for to in AGENT_STATES {
            case += 1;
            let desc = format!("{} -> {}", from.label(), to.label());
            let legal = from == to || from.allowed_transitions().contains(&to);
            let mut machine = StateMachine::new(from);
            let outcome = machine.transition(to);
            if legal {
                assert!(
                    outcome.is_ok(),
                    "state-matrix case #{case} ({desc}): declared legal but refused: {outcome:?}"
                );
                assert_eq!(
                    machine.state(),
                    to,
                    "state-matrix case #{case} ({desc}): machine must land on the target"
                );
            } else {
                let err = outcome.expect_err(&format!(
                    "state-matrix case #{case} ({desc}): illegal transition must be refused"
                ));
                assert!(
                    matches!(err.kind, ErrorKind::InvalidState { from: f, to: t } if f == from && t == to),
                    "state-matrix case #{case} ({desc}): wrong error kind: {err:?}"
                );
                assert_eq!(
                    machine.state(),
                    from,
                    "state-matrix case #{case} ({desc}): refused transition must not move the state"
                );
            }
        }
    }
    assert_eq!(
        case, 289,
        "the full 17x17 agent-state matrix must be covered"
    );
    for terminal in [AgentState::Completed, AgentState::FailedPermanent] {
        case += 1;
        assert!(
            terminal.allowed_transitions().is_empty(),
            "terminal-state case #{case} ({terminal:?}): terminal states must expose no transitions"
        );
        assert!(terminal.is_terminal(), "terminal-state case #{case}");
    }
}

#[test]
fn core_session_lifecycle_full_matrix_is_individually_asserted() {
    let mut case = 0usize;
    for from in LIFECYCLES {
        for to in LIFECYCLES {
            case += 1;
            let legal = from == to || from.allowed_transitions().contains(&to);
            if from == to {
                assert!(
                    legal,
                    "lifecycle-matrix case #{case} ({} -> {}): self transitions are idempotent",
                    from.label(),
                    to.label()
                );
            }
            if from.is_terminal() && from != to {
                assert!(
                    !legal,
                    "lifecycle-matrix case #{case}: terminal {} must not transition to {}",
                    from.label(),
                    to.label()
                );
            }
        }
    }
    assert_eq!(case, 25, "the full 5x5 lifecycle matrix must be covered");
    assert!(SessionLifecycle::Open.can_accept_prompts());
    assert!(!SessionLifecycle::Suspended.can_accept_prompts());
    assert!(!SessionLifecycle::Closing.can_accept_prompts());
    assert!(!SessionLifecycle::Closed.can_accept_prompts());
    assert!(!SessionLifecycle::FailedPermanent.can_accept_prompts());
}

#[test]
fn core_journal_sequence_boundaries_are_individually_asserted() {
    let table: [(Option<EventSeq>, Option<u64>); 6] = [
        (None, Some(1)),
        (Some(EventSeq::new(1)), Some(2)),
        (Some(EventSeq::new(2)), Some(3)),
        (
            Some(EventSeq::new(i64::MAX as u64)),
            Some(i64::MAX as u64 + 1),
        ),
        (Some(EventSeq::new(u64::MAX - 1)), Some(u64::MAX)),
        (Some(EventSeq::new(u64::MAX)), None),
    ];
    let mut case = 0usize;
    for (prev, expected) in table {
        case += 1;
        let got = JournalInvariants::checked_next_seq(prev).map(|s| s.raw());
        assert_eq!(
            got, expected,
            "seq-boundary case #{case} (prev={prev:?}): checked_next_seq must never wrap"
        );
        if let Some(expected) = expected {
            assert_eq!(
                JournalInvariants::next_seq(prev).raw(),
                expected,
                "seq-boundary case #{case} (prev={prev:?}): next_seq must agree with checked_next_seq"
            );
        }
    }
    let overflow = catch_unwind(|| JournalInvariants::next_seq(Some(EventSeq::new(u64::MAX))));
    assert!(
        overflow.is_err(),
        "seq-overflow case: next_seq at u64::MAX must panic loudly, never wrap"
    );
    let zero = EventSeq::try_from(0);
    assert!(
        zero.is_err(),
        "seq-zero case: EventSeq::try_from(0) must be a typed error"
    );
    assert_eq!(
        TaskRevision::new(u64::MAX).checked_next(),
        None,
        "revision-overflow case: TaskRevision::checked_next must stop at u64::MAX"
    );
    assert_eq!(
        TaskRevision::new(1).checked_next().map(|r| r.raw()),
        Some(2),
        "revision case: revision 1 must advance to 2"
    );
    assert_eq!(case, 6, "COUNT");
}

#[test]
fn core_journal_timestamp_matrix_never_decreases() {
    let prevs: [Option<i64>; 7] = [
        None,
        Some(i64::MIN),
        Some(-1),
        Some(0),
        Some(1),
        Some(1_000),
        Some(i64::MAX),
    ];
    let nows: [i64; 6] = [i64::MIN, -1, 0, 1, 1_000, i64::MAX];
    let mut case = 0usize;
    for prev in prevs {
        for now in nows {
            case += 1;
            let got = JournalInvariants::monotonic_ts(prev, now);
            let expected = prev.map_or(now, |p| p.max(now));
            assert_eq!(
                got, expected,
                "ts-matrix case #{case} (prev={prev:?}, now={now}): monotonic_ts must clamp backwards clocks"
            );
            if let Some(p) = prev {
                assert!(
                    got >= p,
                    "ts-matrix case #{case} (prev={prev:?}, now={now}): time must never run backwards"
                );
            }
        }
    }
    assert_eq!(case, 42, "the 7x6 timestamp matrix must be covered");
}

#[test]
fn core_event_vocabulary_and_serde_roundtrips() {
    assert_eq!(
        EventKind::ALL.len(),
        28,
        "event vocabulary must stay at the documented 28 kinds"
    );
    let mut case = 0usize;
    for kind in EventKind::ALL {
        case += 1;
        kind.exhaustive();
        let json = serde_json::to_string(&kind).expect("event kind serializes");
        let back: EventKind = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("event-vocab case #{case} ({kind:?}): {e}"));
        assert_eq!(
            back, kind,
            "event-vocab case #{case} ({kind:?}): serde roundtrip must be identity"
        );
        assert_eq!(
            json,
            serde_json::to_string(&kind).unwrap(),
            "event-vocab case #{case} ({kind:?}): serialization must be deterministic"
        );
    }
    for state in AGENT_STATES {
        case += 1;
        let json = serde_json::to_string(&state).expect("state serializes");
        let back: AgentState = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("state-serde case #{case} ({state:?}): {e}"));
        assert_eq!(
            back, state,
            "state-serde case #{case} ({state:?}): serde roundtrip must be identity"
        );
    }
    for hostile in [
        "\"bogus\"",
        "\"\"",
        "\"SessionCreated\"",
        "null",
        "1",
        "{\"kind\":\"session_created\"}",
    ] {
        case += 1;
        assert!(
            serde_json::from_str::<EventKind>(hostile).is_err(),
            "event-hostile case #{case} ({hostile}): unknown event vocabulary must be refused"
        );
    }
    for hostile in ["\"bogus\"", "\"\"", "null", "7"] {
        case += 1;
        assert!(
            serde_json::from_str::<AgentState>(hostile).is_err(),
            "state-hostile case #{case} ({hostile}): unknown agent state must be refused"
        );
    }
    let event = Event::new(
        EventSeq::new(9),
        SessionId::new(2),
        Some(OpId::new(3)),
        EventKind::PhaseChanged,
        AgentState::ExecutingTool,
        i64::MAX,
        None,
    );
    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(
        serde_json::from_str::<Event>(&json).unwrap(),
        event,
        "event roundtrip with a None payload must be identity"
    );
    assert_eq!(case, 55, "event/state case count drifted");
}

#[test]
fn core_identifier_boundaries_and_serde() {
    macro_rules! id_cases {
        ($($t:ty),+ $(,)?) => {{
            let mut case = 0usize;
            $(
                case += 1;
                let ok = <$t>::try_from(7u64)
                    .unwrap_or_else(|e| panic!("id case #{case} ({}): nonzero must convert: {e}", stringify!($t)));
                assert_eq!(ok.raw(), 7, "id case #{case} ({}): raw must roundtrip", stringify!($t));
                case += 1;
                let zero = <$t>::try_from(0u64);
                assert!(
                    zero.is_err(),
                    "id case #{case} ({}): zero must be a typed error, never a panic",
                    stringify!($t)
                );
                case += 1;
                let max = <$t>::try_from(u64::MAX).expect("u64::MAX is a valid raw id");
                assert_eq!(max.raw(), u64::MAX, "id case #{case} ({}): max raw", stringify!($t));
                case += 1;
                let json = serde_json::to_string(&max).unwrap();
                let back: $t = serde_json::from_str(&json)
                    .unwrap_or_else(|e| panic!("id case #{case} ({}): serde: {e}", stringify!($t)));
                assert_eq!(back, max, "id case #{case} ({}): serde roundtrip", stringify!($t));
                case += 1;
                assert!(
                    serde_json::from_str::<$t>("0").is_err(),
                    "id case #{case} ({}): zero JSON must be refused", stringify!($t)
                );
                case += 1;
                let panic_zero = catch_unwind(|| <$t>::new(0));
                assert!(
                    panic_zero.is_err(),
                    "id case #{case} ({}): new(0) must panic", stringify!($t)
                );
            )+
            case
        }};
    }
    let case = id_cases!(
        SessionId,
        WorkspaceId,
        WorktreeId,
        TaskId,
        VerificationRecordId,
        TaskRevision,
        OpId,
        ProviderCallId,
        EventSeq,
    );
    assert_eq!(case, 54, "nine id types times six boundary checks");
}

#[test]
fn core_mime_and_filename_corpus_is_typed() {
    let long_mime = format!("text/{}", "a".repeat(MAX_ATTACHMENT_MIME_BYTES));
    let mimes: [(&str, bool); 30] = [
        ("text/plain", true),
        ("image/png", true),
        ("application/pdf", true),
        ("application/octet-stream", true),
        ("text/x-rust+md", true),
        ("application/vnd.api+json", true),
        ("audio/mpeg", true),
        ("video/mp4", true),
        ("a/b", true),
        ("application/x-www-form-urlencoded", true),
        ("", false),
        ("   ", false),
        ("text", false),
        ("text/", false),
        ("/plain", false),
        ("text/plain/extra", false),
        ("text /plain", false),
        ("TEXT/PLAIN", false),
        ("Text/Plain", false),
        ("text/pla in", false),
        ("text/pl\u{7}ain", false),
        ("te\txt/plain", false),
        (".text/plain", false),
        ("text/.plain", false),
        ("text/plain\n", false),
        ("tëxt/plain", false),
        ("text/plain ", false),
        ("tex t/plain", false),
        ("text/plain\u{0}", false),
        ("txt/pl@in", false),
    ];
    let mut case = 0usize;
    for (mime, ok) in mimes {
        case += 1;
        let got = validate_mime(mime);
        assert_eq!(
            got.is_ok(),
            ok,
            "mime-corpus case #{case} ({mime:?}): expected ok={ok}, got {got:?}"
        );
    }
    case += 1;
    assert_eq!(
        validate_mime(&long_mime).unwrap_err().kind,
        ErrorKind::Oversized,
        "mime-corpus case #{case}: 129-byte mime must be Oversized"
    );
    case += 1;
    assert!(
        validate_mime("application/json").is_ok(),
        "mime-corpus case #{case}: canonical json must be accepted"
    );

    let long_name = "a".repeat(MAX_ATTACHMENT_FILENAME_BYTES + 1);
    let names: [(&str, bool); 26] = [
        ("a.png", true),
        ("shot-1.jpeg", true),
        ("a", true),
        (".hidden", true),
        ("file name.txt", true),
        ("café.png", true),
        ("UPPER.PNG", true),
        ("with_underscore.rs", true),
        ("digits123", true),
        ("", false),
        (" leading.png", false),
        ("trailing.png ", false),
        (".", false),
        ("..", false),
        ("a/b.png", false),
        ("a\\b.png", false),
        ("bad\u{0}name", false),
        ("bad\nname", false),
        ("bad\rname", false),
        ("bad\u{7f}name", false),
        ("..\\secrets", false),
        ("/etc/passwd", false),
        ("C:evil", true),
        ("\tname", false),
        ("name\t", false),
        ("a\u{202e}gnp.txt", true),
    ];
    for (name, ok) in names {
        case += 1;
        let got = validate_filename(name);
        assert_eq!(
            got.is_ok(),
            ok,
            "filename-corpus case #{case} ({name:?}): expected ok={ok}, got {got:?}"
        );
    }
    case += 1;
    assert_eq!(
        validate_filename(&long_name).unwrap_err().kind,
        ErrorKind::Oversized,
        "filename-corpus case #{case}: 256-byte filename must be Oversized"
    );

    case += 1;
    let id = AttachmentId::new(hash(1), "image/png", Some("ok.png"), MAX_ATTACHMENT_BYTES)
        .expect("boundary size is valid");
    assert!(
        id.is_image(),
        "attachment case #{case}: image mime must classify"
    );
    case += 1;
    let over = AttachmentId::new(hash(2), "image/png", None, MAX_ATTACHMENT_BYTES + 1);
    assert_eq!(
        over.unwrap_err().kind,
        ErrorKind::Oversized,
        "attachment case #{case}: one byte over the payload cap must be Oversized"
    );
    case += 1;
    let zero = AttachmentId::new(hash(3), "text/plain", None, 0).expect("zero bytes is valid");
    assert_eq!(
        zero.size, 0,
        "attachment case #{case}: empty attachment is valid"
    );
    case += 1;
    let ref_zero = AttachmentRef {
        id: 0,
        digest: hash(4),
        mime: "text/plain".into(),
        filename: None,
        size: 1,
    };
    assert_eq!(
        ref_zero.validate().unwrap_err().kind,
        ErrorKind::Malformed,
        "attachment case #{case}: ref id 0 is not a durable row id"
    );
    case += 1;
    assert_eq!(case, 64, "mime/filename/attachment case count drifted");
}

#[test]
fn core_workspace_path_corpus_is_typed() {
    let cases: [(&str, bool); 33] = [
        ("src/a.rs", true),
        ("a", true),
        ("a/b/c", true),
        ("src/", true),
        ("a.b/c-d_e", true),
        ("Café/Ünïcode", true),
        ("a/b/c/d/e/f", true),
        ("", false),
        ("/abs", false),
        ("//server/share", false),
        ("a\\b", false),
        ("a\\", false),
        ("C:/x", false),
        ("c:x", false),
        ("a:b", false),
        ("a//b", false),
        ("a/", true),
        ("a///", false),
        ("a/./b", false),
        ("a/.", false),
        ("./a", false),
        ("a/..", false),
        ("..", false),
        ("../a", false),
        ("a/../b", false),
        ("a\u{0}b", false),
        ("a\nb", false),
        ("NUL", false),
        ("nul.txt", false),
        ("CON", false),
        ("com1/x", false),
        ("a./b", false),
        ("a /b", false),
    ];
    let mut case = 0usize;
    for (raw, ok) in cases {
        case += 1;
        let got = NormalizedWorkspacePath::new(raw);
        assert_eq!(
            got.is_ok(),
            ok,
            "path-corpus case #{case} ({raw:?}): expected ok={ok}, got {got:?}"
        );
        if let Ok(path) = got {
            assert!(
                !path.as_str().starts_with('/') && !path.as_str().contains('\\'),
                "path-corpus case #{case} ({raw:?}): canonical form must be relative with one separator"
            );
            assert!(
                path.covers(&path),
                "path-corpus case #{case}: a path must cover itself"
            );
        }
    }
    case += 1;
    let at_cap = "a".repeat(256);
    assert!(
        NormalizedWorkspacePath::new(&at_cap).is_ok(),
        "path-corpus case #{case}: a 256-byte path is at the cap and must be accepted"
    );
    case += 1;
    let long = "a".repeat(257);
    assert!(
        NormalizedWorkspacePath::new(&long).is_err(),
        "path-corpus case #{case}: a 257-byte path must be refused"
    );
    let a = NormalizedWorkspacePath::new("src").unwrap();
    let b = NormalizedWorkspacePath::new("src/a.rs").unwrap();
    let c = NormalizedWorkspacePath::new("src2/a.rs").unwrap();
    assert!(a.covers(&b), "path-cover: src covers src/a.rs");
    assert!(!a.covers(&c), "path-cover: src must never cover src2/a.rs");
    assert!(
        a.overlaps(&b) && b.overlaps(&a),
        "path-overlap: ancestor touches child"
    );
    assert!(!a.overlaps(&c), "path-overlap: component boundary only");
    assert_eq!(
        NormalizedWorkspacePath::new("src/").unwrap().as_str(),
        "src",
        "path-canonical: one trailing slash is a directory marker"
    );
    assert_eq!(case, 35, "COUNT");
}

#[test]
fn core_ownership_and_change_budget_corpus() {
    let mut case = 0usize;
    let specs: [(OwnershipSpec, bool); 8] = [
        (OwnershipSpec::NoWrites, true),
        (OwnershipSpec::IsolatedWorktree, true),
        (
            OwnershipSpec::Paths {
                paths: vec!["src/a.rs".into()],
            },
            true,
        ),
        (OwnershipSpec::Paths { paths: vec![] }, false),
        (
            OwnershipSpec::Paths {
                paths: vec!["".into()],
            },
            false,
        ),
        (
            OwnershipSpec::Paths {
                paths: vec!["/abs".into()],
            },
            false,
        ),
        (
            OwnershipSpec::Paths {
                paths: vec!["a/../b".into()],
            },
            false,
        ),
        (
            OwnershipSpec::Paths {
                paths: vec!["src".into(), "src/a.rs".into()],
            },
            false,
        ),
    ];
    for (spec, ok) in specs {
        case += 1;
        let got = spec.validate();
        assert_eq!(
            got.is_ok(),
            ok,
            "ownership case #{case} ({spec:?}): expected ok={ok}, got {got:?}"
        );
    }
    let over = OwnershipSpec::Paths {
        paths: (0..=64).map(|i| format!("p{i}")).collect(),
    };
    case += 1;
    assert!(
        over.validate().is_err(),
        "ownership case #{case}: 65 entries exceed MAX_OWNERSHIP_SPEC_ENTRIES"
    );
    let long_entry = OwnershipSpec::Paths {
        paths: vec!["a".repeat(257)],
    };
    case += 1;
    assert!(
        long_entry.validate().is_err(),
        "ownership case #{case}: a 257-char entry exceeds the entry cap"
    );

    let path_a = OwnershipSpec::Paths {
        paths: vec!["src".into()],
    };
    let path_b = OwnershipSpec::Paths {
        paths: vec!["src/a.rs".into()],
    };
    let path_c = OwnershipSpec::Paths {
        paths: vec!["docs/x.md".into()],
    };
    assert!(
        path_a.overlaps(&path_b),
        "ownership-overlap: subtree collision"
    );
    assert!(
        !path_a.overlaps(&path_c),
        "ownership-overlap: disjoint paths"
    );
    assert!(
        !path_a.overlaps(&OwnershipSpec::IsolatedWorktree),
        "ownership-overlap: isolated worktree never collides"
    );
    assert!(
        !path_a.overlaps(&OwnershipSpec::NoWrites),
        "ownership-overlap: NoWrites never collides"
    );
    let sem_a = OwnershipSpec::SemanticEntities {
        provider_id: "p".into(),
        snapshot_id: "s".into(),
        entities: vec!["e1".into()],
    };
    let sem_b = OwnershipSpec::SemanticEntities {
        provider_id: "p".into(),
        snapshot_id: "s".into(),
        entities: vec!["e1".into(), "e2".into()],
    };
    let sem_c = OwnershipSpec::SemanticEntities {
        provider_id: "p".into(),
        snapshot_id: "other".into(),
        entities: vec!["e1".into()],
    };
    assert!(
        sem_a.overlaps(&sem_b),
        "ownership-overlap: shared snapshot+entity"
    );
    assert!(
        !sem_a.overlaps(&sem_c),
        "ownership-overlap: different snapshot is disjoint"
    );
    assert!(
        !sem_a.overlaps(&path_a),
        "ownership-overlap: channels never collide statically"
    );

    let budget = ChangeBudget {
        allowed_paths: vec!["src".into(), "docs/".into()],
        allowed_semantic_entities: vec!["entity:1".into()],
        ..ChangeBudget::default()
    };
    let budget_cases: [(&str, bool); 8] = [
        ("src/a.rs", true),
        ("src", true),
        ("src2/a.rs", false),
        ("docs/x.md", true),
        ("/src/a.rs", false),
        ("SRC/a.rs", false),
        ("src/a/../b", true),
        ("other/file", false),
    ];
    for (path, ok) in budget_cases {
        case += 1;
        assert_eq!(
            budget.allows_path(path),
            ok,
            "change-budget case #{case} ({path:?}): expected allows={ok}"
        );
    }
    case += 1;
    assert!(
        budget.allows_semantic_entity("entity:1"),
        "change-budget case #{case}: listed entity must be allowed"
    );
    case += 1;
    assert!(
        !budget.allows_semantic_entity("entity:2"),
        "change-budget case #{case}: unlisted entity must be refused"
    );
    case += 1;
    assert!(
        ChangeBudget::default().allows_path("anywhere/at/all"),
        "change-budget case #{case}: empty path set is unrestricted"
    );
    case += 1;
    let restricted = ChangeBudget {
        allowed_semantic_entities: vec!["entity:1".into()],
        ..ChangeBudget::default()
    };
    assert!(
        !restricted.allows_semantic_entity(""),
        "change-budget case #{case}: an empty entity is never allowed by a non-empty set"
    );
    assert_eq!(
        crate::state::norm_budget_path("./a\\b/"),
        "a/b",
        "change-budget: normalization order"
    );
    assert_eq!(case, 22, "COUNT");
}

#[test]
fn core_criterion_binding_corpus() {
    let mut case = 0usize;
    let valid: [CriterionBinding; 7] = [
        CriterionBinding::RequiredCheck {
            check_id: "make_test".into(),
            command_digest: "blake3:".to_string() + &"a".repeat(64),
        },
        CriterionBinding::IntegrationCoverage {
            required_work_items: vec!["w1".into(), "w2".into()],
        },
        CriterionBinding::FileState {
            path: "src/a.rs".into(),
            expected_digest: "blake3:".to_string() + &"b".repeat(64),
        },
        CriterionBinding::Evidence {
            evidence_id: "7".into(),
            evidence_digest: "blake3:".to_string() + &"c".repeat(64),
        },
        CriterionBinding::IndependentReview {
            reviewer_id: "reviewer-1".into(),
        },
        CriterionBinding::AggregateGoal,
        CriterionBinding::Unavailable {
            reason: "no resolution".into(),
        },
    ];
    for binding in valid {
        case += 1;
        assert!(
            binding.validate().is_ok(),
            "criterion case #{case} ({:?}): valid binding must validate",
            binding.kind_label()
        );
        assert_eq!(
            binding.content_digest(),
            binding.content_digest(),
            "criterion case #{case}: content digest must be deterministic"
        );
        let json = serde_json::to_string(&binding).unwrap();
        let back: CriterionBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(back, binding, "criterion case #{case}: serde roundtrip");
    }
    let oversized = CriterionBinding::Unavailable {
        reason: "x".repeat(513),
    };
    case += 1;
    assert!(
        oversized.validate().is_err(),
        "criterion case #{case}: 513-byte reason must be refused"
    );
    let too_many = CriterionBinding::IntegrationCoverage {
        required_work_items: (0..65).map(|i| format!("w{i}")).collect(),
    };
    case += 1;
    assert!(
        too_many.validate().is_err(),
        "criterion case #{case}: 65 work items exceed the bound"
    );
    case += 1;
    assert_eq!(
        legacy_binding_for_criterion_text("goal: ship it"),
        CriterionBinding::AggregateGoal,
        "criterion case #{case}: canonical goal text migrates to AggregateGoal"
    );
    case += 1;
    match legacy_binding_for_criterion_text("required check: make test") {
        CriterionBinding::RequiredCheck { check_id, .. } => {
            assert!(
                check_id.is_empty(),
                "criterion case #{case}: legacy check has no id"
            );
        }
        other => panic!("criterion case #{case}: expected RequiredCheck, got {other:?}"),
    }
    case += 1;
    assert!(
        matches!(
            legacy_binding_for_criterion_text("free text"),
            CriterionBinding::Unavailable { .. }
        ),
        "criterion case #{case}: non-canonical text is honestly Unavailable"
    );
    case += 1;
    assert!(
        !legacy_binding_for_criterion_text("free text")
            .content_digest()
            .is_empty(),
        "criterion case #{case}: an unavailable binding still digests"
    );
    let checkpoint: [(NoOpDisposition, bool, bool); 6] = [
        (NoOpDisposition::Allowed, false, true),
        (NoOpDisposition::Allowed, true, true),
        (NoOpDisposition::RequiresCriterionProof, false, false),
        (NoOpDisposition::RequiresCriterionProof, true, true),
        (NoOpDisposition::Refused, false, false),
        (NoOpDisposition::Refused, true, false),
    ];
    for (disposition, criterion_proof, allowed) in checkpoint {
        case += 1;
        assert_eq!(
            disposition.completion_allowed(criterion_proof),
            allowed,
            "no-op case #{case} ({disposition:?}, proof={criterion_proof}): wrong completion verdict"
        );
    }
    case += 1;
    assert!(
        NoOpDisposition::RequiresCriterionProof.permits_completion()
            && NoOpDisposition::Allowed.permits_completion()
            && !NoOpDisposition::Refused.permits_completion(),
        "no-op case #{case}: permits_completion table"
    );
    assert_eq!(case, 20, "COUNT");
}

#[test]
fn core_authority_domain_separation_and_classification() {
    let domains: [(&[u8], &str); 13] = [
        (DOMAIN_CRITERION_BINDING, "criterion-binding"),
        (DOMAIN_COMMAND_BINDING, "command-binding"),
        (DOMAIN_TASK_CONTRACT, "task-contract"),
        (DOMAIN_INTEGRATION_SOURCES, "integration-sources"),
        (DOMAIN_CHANGED_FILES, "changed-files"),
        (DOMAIN_CHANGE_SET, "change-set"),
        (DOMAIN_BASE_MAP, "base-map"),
        (DOMAIN_RUN_BASE_MANIFEST, "run-base-manifest"),
        (DOMAIN_CHECK_BASIS, "check-basis"),
        (DOMAIN_CHECK_EXECUTION, "check-execution"),
        (DOMAIN_ACCOUNTING_BALANCE, "accounting-balance"),
        (DOMAIN_SEMANTIC_FACT, "semantic-fact"),
        (DOMAIN_CANDIDATE_MANIFEST, "candidate-manifest"),
    ];
    let mut seen = std::collections::HashSet::new();
    let mut case = 0usize;
    for (domain, label) in domains {
        case += 1;
        let digest = authority_digest_labeled(domain, 1, Fields::new().text("payload"));
        assert!(
            digest.starts_with("blake3:") && digest.len() == 7 + 64,
            "domain case #{case} ({label}): labelled digest shape"
        );
        assert!(
            seen.insert(digest.clone()),
            "domain case #{case} ({label}): domain separation must be injective, got a collision"
        );
    }
    case += 1;
    assert_ne!(
        authority_digest_labeled(DOMAIN_BASE_MAP, 1, Fields::new().text("x")),
        authority_digest_labeled(DOMAIN_BASE_MAP, 2, Fields::new().text("x")),
        "domain case #{case}: version must separate digests"
    );
    let splits = [
        (vec!["ab", "c"], vec!["a", "bc"]),
        (vec!["", "ab"], vec!["ab", ""]),
        (vec!["a/b"], vec!["a", "b"]),
    ];
    for (left, right) in splits {
        case += 1;
        let mut l = Fields::new();
        for item in &left {
            l = l.text(item);
        }
        let mut r = Fields::new();
        for item in &right {
            r = r.text(item);
        }
        assert_ne!(
            authority_digest_labeled(DOMAIN_CHANGE_SET, 1, l),
            authority_digest_labeled(DOMAIN_CHANGE_SET, 1, r),
            "domain case #{case}: field boundaries must be injective ({left:?} vs {right:?})"
        );
    }
    case += 1;
    assert_ne!(
        authority_digest_labeled(DOMAIN_CHANGE_SET, 1, Fields::new().opt_text(None)),
        authority_digest_labeled(DOMAIN_CHANGE_SET, 1, Fields::new().opt_text(Some(""))),
        "domain case #{case}: None must never alias Some(\"\")"
    );
    case += 1;
    assert_ne!(
        authority_digest_labeled(DOMAIN_CHANGE_SET, 1, Fields::new().uint(0)),
        authority_digest_labeled(DOMAIN_CHANGE_SET, 1, Fields::new().text("0")),
        "domain case #{case}: numeric and text fields must not alias"
    );
    let field_cap = std::hint::black_box(MAX_AUTHORITY_FIELD_BYTES);
    assert!(
        field_cap >= 1024 * 1024,
        "domain: the authority field cap must stay at least 1 MiB"
    );
    let valid_hex = "a".repeat(64);
    let classify: Vec<(String, AuthorityDigestKind)> = vec![
        (String::new(), AuthorityDigestKind::Empty),
        (
            format!("blake3:{valid_hex}"),
            AuthorityDigestKind::Blake3Labeled,
        ),
        (valid_hex.clone(), AuthorityDigestKind::Blake3Hex),
        (
            "fnv1a64:0123456789abcdef".to_string(),
            AuthorityDigestKind::LegacyFnv,
        ),
        (
            "accounting:v1:0123456789abcdef".to_string(),
            AuthorityDigestKind::LegacyFnv,
        ),
        (
            "0123456789abcdef".to_string(),
            AuthorityDigestKind::LegacyFnv,
        ),
        ("blake3:".to_string(), AuthorityDigestKind::Unknown),
        ("blake3:short".to_string(), AuthorityDigestKind::Unknown),
        (
            format!("blake3:{}", "A".repeat(64)),
            AuthorityDigestKind::Unknown,
        ),
        (
            format!("blake3:{}", "g".repeat(64)),
            AuthorityDigestKind::Unknown,
        ),
        (
            format!("sha256:{}", "0".repeat(64)),
            AuthorityDigestKind::Unknown,
        ),
        ("fnv1a64:".to_string(), AuthorityDigestKind::Unknown),
        ("fnv1a64:zzzz".to_string(), AuthorityDigestKind::Unknown),
        ("accounting:v1:".to_string(), AuthorityDigestKind::Unknown),
        (
            "accounting:v2:0123456789abcdef".to_string(),
            AuthorityDigestKind::Unknown,
        ),
        ("0123456789abcde".to_string(), AuthorityDigestKind::Unknown),
        (
            "0123456789abcdef0".to_string(),
            AuthorityDigestKind::Unknown,
        ),
        ("0123456789ABCDEF".to_string(), AuthorityDigestKind::Unknown),
        ("not-a-digest".to_string(), AuthorityDigestKind::Unknown),
        (
            format!("blake3:{}", "0".repeat(63)),
            AuthorityDigestKind::Unknown,
        ),
        (
            format!("BLAKE3:{}", "0".repeat(64)),
            AuthorityDigestKind::Unknown,
        ),
        (" ".to_string(), AuthorityDigestKind::Unknown),
    ];
    let mut classify_case = 0usize;
    for (value, expected) in &classify {
        classify_case += 1;
        case += 1;
        assert_eq!(
            classify_authority_digest(value),
            *expected,
            "classify case #{classify_case} ({value:?}): wrong authority class"
        );
    }
    assert!(
        AuthorityDigestKind::Blake3Labeled.is_authoritative()
            && AuthorityDigestKind::Blake3Hex.is_authoritative()
            && !AuthorityDigestKind::LegacyFnv.is_authoritative()
            && !AuthorityDigestKind::Unknown.is_authoritative()
            && !AuthorityDigestKind::Empty.is_authoritative()
    );
    case += 1;
    assert!(
        refuse_legacy_authority_digest("change-set", "0123456789abcdef").is_err(),
        "refuse-legacy case #{case}: FNV must fail closed"
    );
    case += 1;
    assert!(
        refuse_legacy_authority_digest("change-set", &valid_hex).is_ok(),
        "refuse-legacy case #{case}: canonical BLAKE3 passes"
    );
    case += 1;
    assert!(
        refuse_legacy_authority_digest("change-set", "foreign:value").is_ok(),
        "refuse-legacy case #{case}: the caller's own shape validation owns unknown values"
    );
    case += 1;
    let legacy = refuse_legacy_authority_digest("base-map", "fnv1a64:0123456789abcdef")
        .expect_err("legacy digest must be refused");
    assert_eq!(
        legacy.what, "base-map",
        "refuse-legacy case #{case}: names the field"
    );
    assert_eq!(
        legacy.value, "fnv1a64:0123456789abcdef",
        "refuse-legacy case #{case}: preserves the value"
    );
    assert_eq!(case, 45, "COUNT");
}

#[test]
fn core_file_hash_boundaries() {
    let mut case = 0usize;
    for byte in 0..32u8 {
        case += 1;
        let h = hash(byte);
        let hex = h.to_hex();
        assert_eq!(
            hex.len(),
            64,
            "hash case #{case} (byte {byte}): hex must be 64 chars"
        );
        assert_eq!(
            FileHash::from_hex(&hex),
            Some(h),
            "hash case #{case} (byte {byte}): from_hex(to_hex(h)) must be identity"
        );
        assert!(
            h.cas_path().starts_with(&hex[..2]),
            "hash case #{case} (byte {byte}): CAS path must be sharded by the first bytes"
        );
    }
    let invalid: Vec<String> = vec![
        String::new(),
        "0".to_string(),
        "a".repeat(63),
        "a".repeat(65),
        "g".repeat(64),
        "z".repeat(64),
        "not-hex-at-all-not-hex-at-all-not-hex-at-all-not-hex-at-all-not-he".to_string(),
        format!("{}g", "0".repeat(63)),
        format!(" {}", "0".repeat(64)),
        format!("{} ", "0".repeat(64)),
        format!("-{}", "0".repeat(63)),
        format!("0x{}", "0".repeat(62)),
        "\u{0}".to_string(),
        "é".to_string(),
        format!("{}-", "0".repeat(63)),
    ];
    for value in &invalid {
        case += 1;
        assert_eq!(
            FileHash::from_hex(value),
            None,
            "hash-hostile case #{case} ({value:?}): malformed hex must be refused"
        );
    }
    case += 1;
    assert_eq!(
        FileHash::from_hex(&"0".repeat(64)),
        Some(hash(0)),
        "hash case #{case}: all-zero digest is a valid BLAKE3 value"
    );
    case += 1;
    assert_eq!(
        FileHash::from_hex(&"A".repeat(64)),
        Some(FileHash::from([0xAA; 32])),
        "hash case #{case}: hex parsing is case-insensitive by documented behavior"
    );
    assert_eq!(case, 49, "COUNT");
}

#[test]
fn core_deadline_and_op_meta_boundaries() {
    let deadlines: [(i64, i64, bool); 12] = [
        (0, -1, false),
        (0, 0, true),
        (0, 1, true),
        (10, 9, false),
        (10, 10, true),
        (10, 11, true),
        (i64::MAX, i64::MAX - 1, false),
        (i64::MAX, i64::MAX, true),
        (i64::MIN, i64::MIN, true),
        (i64::MIN, i64::MIN + 1, true),
        (-5, -6, false),
        (-5, -5, true),
    ];
    let mut case = 0usize;
    for (at, now, expired) in deadlines {
        case += 1;
        assert_eq!(
            Deadline::at(at).is_expired(now),
            expired,
            "deadline case #{case} (at={at}, now={now}): inclusive boundary"
        );
    }
    let clock = TestClock::new(1_000);
    assert_eq!(clock.now_ms(), 1_000, "clock case: initial reading");
    clock.advance(-2_000);
    assert_eq!(
        clock.now_ms(),
        -1_000,
        "clock case: backward skew is representable"
    );
    clock.set(42);
    assert_eq!(clock.now_ms(), 42, "clock case: set");
    assert_eq!(
        Deadline::now_plus(&TestClock::new(i64::MAX), u64::MAX).at_ms(),
        i64::MAX,
        "deadline case: now_plus must saturate"
    );
    assert_eq!(
        Deadline::now_plus(&TestClock::new(i64::MIN), u64::MAX).at_ms(),
        i64::MIN.saturating_add(i64::MAX),
        "deadline case: now_plus at i64::MIN saturates additions"
    );

    let op_cases: [(bool, i64, i64, Option<ErrorKind>); 5] = [
        (false, 100, 99, None),
        (false, 100, 100, Some(ErrorKind::Timeout)),
        (false, 100, 101, Some(ErrorKind::Timeout)),
        (true, i64::MAX, 0, Some(ErrorKind::Cancelled)),
        (true, 0, i64::MAX, Some(ErrorKind::Cancelled)),
    ];
    for (cancel, deadline, now, expected) in op_cases {
        case += 1;
        let token = crate::cancellation::CancellationToken::new();
        if cancel {
            token.cancel();
        }
        let meta = OpMeta::new(
            OpId::new(1),
            SessionId::new(1),
            Deadline::at(deadline),
            RetryPolicy::default(),
            token,
            RecoveryStrategy::None,
            now,
        );
        let got = meta.ensure_alive(now);
        match expected {
            None => assert!(
                got.is_ok(),
                "op-meta case #{case}: live op must pass ensure_alive, got {got:?}"
            ),
            Some(kind) => {
                let err = got.expect_err(&format!("op-meta case #{case}: must fail"));
                assert_eq!(err.kind, kind, "op-meta case #{case}: wrong failure kind");
            }
        }
        case += 1;
        assert_eq!(
            meta.retryable(),
            meta.retry_policy.max_attempts > 1,
            "op-meta case #{case}: retryable() must mirror max_attempts"
        );
    }
    case += 1;
    let replay = OpMeta::new(
        OpId::new(2),
        SessionId::new(1),
        Deadline::at(10),
        RetryPolicy::default(),
        crate::cancellation::CancellationToken::new(),
        RecoveryStrategy::Idempotent,
        0,
    )
    .with_replay(serde_json::json!({"tool":"read"}));
    assert!(
        replay.replay.is_some(),
        "op-meta case #{case}: replay descriptor attaches"
    );
    case += 1;
    assert!(
        ModelCallAttempt::new(OpId::new(1), OpId::new(1), 0).is_none(),
        "attempt case #{case}: an attempt must never reuse its logical op id"
    );
    case += 1;
    let attempt = ModelCallAttempt::new(OpId::new(1), OpId::new(2), u32::MAX)
        .expect("distinct ids form an attempt");
    assert_eq!(
        attempt.ordinal,
        u32::MAX,
        "attempt case #{case}: ordinal roundtrips"
    );
    assert_eq!(case, 25, "COUNT");
}

#[test]
fn core_retry_policy_matrix() {
    let classes = [
        RetryClass::Network,
        RetryClass::RateLimited,
        RetryClass::ServerError,
        RetryClass::Always,
    ];
    let flags = [(true, false), (true, true), (false, false), (false, true)];
    let mut case = 0usize;
    for class in classes {
        for max_attempts in [1u32, 2, 5] {
            for attempt in [0u32, 1, 4, u32::MAX] {
                for (retryable, rate_limited) in flags {
                    case += 1;
                    let policy = RetryPolicy {
                        max_attempts,
                        base_delay_ms: 10,
                        max_delay_ms: 100,
                        jitter: 0.0,
                        class,
                    };
                    let got = policy.should_retry(attempt, retryable, rate_limited);
                    let within_attempts = attempt.saturating_add(1) < max_attempts;
                    let class_allows = match class {
                        RetryClass::Network => !rate_limited,
                        RetryClass::RateLimited | RetryClass::ServerError | RetryClass::Always => {
                            true
                        }
                    };
                    let expected = retryable && within_attempts && class_allows;
                    assert_eq!(
                        got, expected,
                        "retry-matrix case #{case} (class={class:?}, max={max_attempts}, attempt={attempt}, retryable={retryable}, rate_limited={rate_limited}): wrong verdict"
                    );
                }
            }
        }
    }
    assert_eq!(case, 192, "the retry matrix must stay fully covered");
    let exact: [(u64, u64, u32, u64); 4] = [
        (100, 1_000, 0, 100),
        (100, 1_000, 1, 200),
        (100, 1_000, 2, 400),
        (100, 1_000, 10, 1_000),
    ];
    for (base, max, attempt, expected) in exact {
        case += 1;
        let policy = RetryPolicy {
            max_attempts: 32,
            base_delay_ms: base,
            max_delay_ms: max,
            jitter: 0.0,
            class: RetryClass::Always,
        };
        assert_eq!(
            policy.next_delay(attempt).as_millis() as u64,
            expected,
            "retry-delay case #{case} (attempt={attempt}): deterministic midpoint"
        );
    }
    let mut lcg = Lcg::new(7);
    for _ in 0..24 {
        case += 1;
        let attempt = (lcg.next() % 64) as u32;
        let policy = RetryPolicy {
            max_attempts: 100,
            base_delay_ms: 1,
            max_delay_ms: 5_000,
            jitter: 0.75,
            class: RetryClass::Always,
        };
        let d = policy.next_delay_rng(attempt, &mut rand::rng()).as_millis() as u64;
        assert!(
            (1..=10_000).contains(&d),
            "retry-delay case #{case} (attempt={attempt}): delay {d} escaped the bounded envelope"
        );
    }
    case += 1;
    assert!(
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        }
        .next_delay(u32::MAX)
        .as_millis()
            >= 1,
        "retry-delay case #{case}: overflow attempts still yield at least 1ms"
    );
}

#[test]
fn core_resource_gauge_and_capability_sets() {
    let mut case = 0usize;
    let limits = ResourceLimits::default();
    for class in ResourceClass::ALL {
        let max = limits.get(class);
        case += 1;
        assert!(
            max >= 1,
            "resource case #{case} ({class:?}): every class has a positive limit"
        );
        let mut gauge = ResourceGauge::new();
        for _ in 0..max {
            assert!(
                gauge.try_acquire(class, &limits).is_ok(),
                "resource case #{case} ({class:?}): capacity {max} must admit every slot"
            );
        }
        assert!(
            gauge.try_acquire(class, &limits).is_err(),
            "resource case #{case} ({class:?}): one past capacity must fail fast"
        );
        assert_eq!(
            gauge.usage(class),
            max,
            "resource case #{case} ({class:?}): usage must equal the acquired count"
        );
        gauge.release(class);
        assert_eq!(
            gauge.usage(class),
            max - 1,
            "resource case #{case} ({class:?}): release must free exactly one slot"
        );
        assert!(
            gauge.try_acquire(class, &limits).is_ok(),
            "resource case #{case} ({class:?}): a released slot is reusable"
        );
    }
    case += 1;
    assert_eq!(
        limits.get(ResourceClass::Model),
        1,
        "resource case #{case}: model serialized"
    );
    case += 1;
    assert_eq!(
        limits.get(ResourceClass::DiskRead),
        16,
        "resource case #{case}: reader budget"
    );
    case += 1;
    let mut gauge = ResourceGauge::new();
    for expected in 0..limits.get(ResourceClass::Cpu) {
        case += 1;
        assert_eq!(
            gauge.usage(ResourceClass::Cpu),
            expected,
            "resource case #{case}: usage starts at zero and increments"
        );
        gauge.try_acquire(ResourceClass::Cpu, &limits).unwrap();
    }

    assert_eq!(CapabilityKind::ALL.len(), 6, "six capability kinds");
    for kind in CapabilityKind::ALL {
        case += 1;
        assert_eq!(
            CapabilityKind::from_name(kind.as_str()),
            Some(kind),
            "capability case #{case} ({kind:?}): tag roundtrip"
        );
    }
    for hostile in ["", "READ", "filesystem", "shell ", "net:read", "read_write"] {
        case += 1;
        assert_eq!(
            CapabilityKind::from_name(hostile),
            None,
            "capability case #{case} ({hostile:?}): unknown capability must not parse"
        );
    }
    let read = CapabilitySet::of(CapabilityKind::Read);
    let write = CapabilitySet::of(CapabilityKind::Write);
    case += 1;
    assert!(
        read.contains(CapabilityKind::Read),
        "capability case #{case}: of() contains"
    );
    case += 1;
    assert!(!read.is_empty(), "capability case #{case}: non-empty set");
    case += 1;
    assert!(
        CapabilitySet::empty().is_empty(),
        "capability case #{case}: empty set"
    );
    case += 1;
    assert_eq!(
        CapabilitySet::all(),
        CapabilitySet::from_kinds(&CapabilityKind::ALL),
        "capability case #{case}: all() equals from_kinds(ALL)"
    );
    case += 1;
    let union = read.union(write);
    assert!(
        union.contains(CapabilityKind::Read) && union.contains(CapabilityKind::Write),
        "capability case #{case}: union carries both"
    );
    case += 1;
    assert!(
        read.intersection(write).is_empty(),
        "capability case #{case}: disjoint intersection"
    );
    case += 1;
    assert!(
        read.is_subset_of(union) && !union.is_subset_of(read),
        "capability case #{case}: subset direction"
    );
    case += 1;
    assert_eq!(
        read.kinds().count(),
        1,
        "capability case #{case}: kinds iterator"
    );

    let deny = NetworkPolicy::DenyAll;
    case += 1;
    assert!(
        !deny.allows("https://api.example.com"),
        "network case #{case}: deny-all refuses all"
    );
    let providers = NetworkPolicy::AllowProviders {
        endpoints: vec!["https://api.example.com".into()],
    };
    case += 1;
    assert!(
        providers.allows("https://api.example.com/v1"),
        "network case #{case}: prefix allowed"
    );
    case += 1;
    assert!(
        !providers.allows("https://evilapi.example.com/v1"),
        "network case #{case}: prefix matching must not admit a longer host"
    );
    case += 1;
    assert!(
        !providers.allows("http://api.example.com"),
        "network case #{case}: scheme matters"
    );
    let configured = NetworkPolicy::AllowConfigured {
        endpoints: vec!["https://api.example.com".into()],
        domains: vec!["https://docs.example.org".into()],
    };
    case += 1;
    assert!(
        configured.allows("https://docs.example.org/x"),
        "network case #{case}: domain allowed"
    );
    case += 1;
    assert!(
        !configured.allows("https://other.example.org"),
        "network case #{case}: unlisted refused"
    );
    assert_eq!(case, 40, "COUNT");
}

#[test]
fn core_blocker_and_phase_vocabulary() {
    let mut case = 0usize;
    for kind in BlockerKind::ALL {
        case += 1;
        assert_eq!(
            BlockerKind::parse(kind.as_str()),
            Some(kind),
            "blocker case #{case} ({kind:?}): parse(as_str) roundtrip"
        );
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(
            serde_json::from_str::<BlockerKind>(&json).unwrap(),
            kind,
            "blocker case #{case}: serde roundtrip"
        );
    }
    for hostile in [
        "",
        "Blocked",
        "dependency ",
        "zombie",
        "permission:1",
        "UNKNOWN",
    ] {
        case += 1;
        assert_eq!(
            BlockerKind::parse(hostile),
            None,
            "blocker case #{case} ({hostile:?}): unknown durable tag must not decode"
        );
    }
    for phase in ExecutionPhase::ALL {
        case += 1;
        assert_eq!(
            ExecutionPhase::parse(phase.as_str()),
            Some(phase),
            "phase case #{case} ({phase:?}): tag roundtrip"
        );
        let json = serde_json::to_string(&phase).unwrap();
        assert_eq!(
            serde_json::from_str::<ExecutionPhase>(&json).unwrap(),
            phase,
            "phase case #{case}: serde roundtrip"
        );
    }
    for hostile in ["", "Planning", "coding ", "verify", "build", "settle"] {
        case += 1;
        assert_eq!(
            ExecutionPhase::parse(hostile),
            None,
            "phase case #{case} ({hostile:?}): unknown phase must not decode"
        );
    }
    assert_eq!(CHILD_LIFECYCLE_TAGS.len(), 7, "seven lifecycle tags");
    for tag in CHILD_LIFECYCLE_TAGS {
        case += 1;
        assert!(
            child_lifecycle_tag_is_known(tag),
            "child case #{case} ({tag}): known tag"
        );
    }
    for hostile in ["", "Running", "idle", "block", "done ", "completed"] {
        case += 1;
        assert!(
            !child_lifecycle_tag_is_known(hostile),
            "child case #{case} ({hostile:?}): unknown tag refused"
        );
    }
    let blocker = ChildBlocker::new(BlockerKind::External, "waiting on CI", "retry later");
    case += 1;
    assert!(
        validate_child_runtime_state("blocked", Some(&blocker)).is_ok(),
        "child case #{case}: blocked with a blocker is valid"
    );
    case += 1;
    assert!(
        validate_child_runtime_state("blocked", None).is_err(),
        "child case #{case}: blocked without a blocker is corrupt"
    );
    case += 1;
    assert!(
        validate_child_runtime_state("running", Some(&blocker)).is_err(),
        "child case #{case}: a non-blocked state carrying a blocker is corrupt"
    );
    case += 1;
    assert!(
        validate_child_runtime_state("running", None).is_ok(),
        "child case #{case}: running without a blocker is valid"
    );
    case += 1;
    let missing = ChildBlocker::dependency(
        &"d".repeat(MAX_CHILD_BLOCKER_DEPENDENCY_CHARS + 1),
        "r",
        "s",
    );
    assert!(
        missing.validate().is_err(),
        "child case #{case}: oversized dependency id must be refused"
    );
    case += 1;
    let long_reason = ChildBlocker::new(
        BlockerKind::Budget,
        "r".repeat(MAX_CHILD_BLOCKER_REASON_CHARS + 1),
        "s",
    );
    assert!(
        long_reason.validate().is_err(),
        "child case #{case}: oversized reason must be refused"
    );
    case += 1;
    let mut control = ChildBlocker::new(BlockerKind::Budget, "ok", "ok");
    control.last_progress_ms = Some(-1);
    assert!(
        control.validate().is_err(),
        "child case #{case}: negative last_progress_ms must be refused"
    );
    case += 1;
    assert!(
        ChildBlocker::new(BlockerKind::Budget, "ok", "ok")
            .validate()
            .is_ok(),
        "child case #{case}: a bounded blocker validates"
    );
    assert_eq!(case, 47, "COUNT");
}

#[test]
fn core_property_deterministic_lcg() {
    let mut case = 0usize;
    for seed in 0..32u64 {
        let mut lcg = Lcg::new(seed.wrapping_add(1));
        let raw = lcg.next() | 1;

        case += 1;
        let id = SessionId::try_from(raw).expect("lcg raw is nonzero");
        assert_eq!(
            id.raw(),
            raw,
            "id-property case #{case} (seed={seed}): raw roundtrip"
        );
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionId>(&json).unwrap(),
            id,
            "id-property case #{case} (seed={seed}): serde roundtrip"
        );

        case += 1;
        let next = JournalInvariants::next_seq(Some(EventSeq::try_from(raw).unwrap()));
        assert_eq!(
            next.raw(),
            raw + 1,
            "seq-property case #{case} (seed={seed}): next seq is exactly prev+1"
        );

        case += 1;
        let prev = Some(raw as i64);
        let now = lcg.next() as i64;
        let ts = JournalInvariants::monotonic_ts(prev, now);
        assert!(
            ts >= raw as i64 && ts >= now,
            "ts-property case #{case} (seed={seed}): monotonic timestamp must dominate both inputs"
        );

        case += 1;
        let digest = FileHash::from({
            let mut bytes = [0u8; 32];
            for chunk in bytes.chunks_mut(8) {
                chunk.copy_from_slice(&lcg.next().to_le_bytes());
            }
            bytes
        });
        assert_eq!(
            FileHash::from_hex(&digest.to_hex()),
            Some(digest),
            "hash-property case #{case} (seed={seed}): hex roundtrip"
        );

        case += 1;
        let left = format!("prog arg{}", lcg.next() % 1_000);
        let right = format!("prog {}", lcg.next() % 1_000);
        if left != right {
            assert_ne!(
                command_binding_digest(&left),
                command_binding_digest(&right),
                "command-property case #{case} (seed={seed}): distinct command texts must not collide ({left:?} vs {right:?})"
            );
        }
        let parts_digest = command_binding_digest_parts(
            "prog",
            &[format!("arg{}", lcg.next() % 1_000)],
            None,
            &[],
        );
        assert!(
            parts_digest.starts_with("blake3:") && parts_digest.len() == 71,
            "command-property case #{case} (seed={seed}): structured digest shape"
        );

        case += 1;
        let rendered = canonical_command_text("prog", &["a".into(), "b".into()]);
        assert_eq!(
            command_binding_digest(&rendered),
            command_binding_digest_parts("prog", &["a".into(), "b".into()], None, &[]),
            "command-property case #{case} (seed={seed}): text and structured digests must agree"
        );

        case += 1;
        let path = NormalizedWorkspacePath::new(&format!("src/f{}.rs", lcg.next() % 1_000))
            .expect("generated path is canonical");
        assert!(
            path.covers(&path) && path.overlaps(&path),
            "path-property case #{case} (seed={seed}): coverage is reflexive"
        );
        let nested = NormalizedWorkspacePath::new(&format!("{}/child", path.as_str()))
            .expect("nested path is canonical");
        assert!(
            path.covers(&nested) && !nested.covers(&path),
            "path-property case #{case} (seed={seed}): ancestor covers child, never the reverse"
        );

        case += 1;
        let a = CapabilitySet::from_kinds(&CapabilityKind::ALL[..(raw % 7) as usize]);
        let b = CapabilitySet::from_kinds(&CapabilityKind::ALL[(raw % 7) as usize..]);
        assert!(
            a.is_subset_of(a.union(b)) && b.is_subset_of(a.union(b)),
            "capability-property case #{case} (seed={seed}): union dominates both operands"
        );

        case += 1;
        let mut rng = Lcg::new(lcg.next());
        let policy = RetryPolicy {
            max_attempts: (rng.next() % 8 + 1) as u32,
            base_delay_ms: rng.next() % 50 + 1,
            max_delay_ms: rng.next() % 500 + 10,
            jitter: 1.0,
            class: RetryClass::Always,
        };
        let delay = policy.next_delay_rng(0, &mut rand::rng()).as_millis() as u64;
        assert!(
            delay >= 1 && delay <= policy.max_delay_ms * 2,
            "retry-property case #{case} (seed={seed}): delay {delay} outside [1, 2*max]"
        );
    }
    assert_eq!(
        case, 288,
        "nine properties across 32 deterministic seeds; count drifted"
    );
}
