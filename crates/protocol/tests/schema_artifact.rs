//! Canonical protocol schema drift gate (audit 25): the checked-in artifact
//! must be byte-identical to what the emitter produces, and the constants it
//! carries must be derived from the frozen `from_core` behavior.

use faktor_core::error::{Error, ErrorKind};
use faktor_protocol::schema::{canonical_json, error_codes, SCHEMA_ID};

#[test]
fn checked_in_artifact_matches_the_emitter() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("schema")
        .join("faktor-protocol.schema.json");
    let existing = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    assert_eq!(
        existing,
        canonical_json(),
        "{} drifted from the emitter; regenerate with \
         `cargo run -p faktor-protocol --bin faktor-protocol-schema -- --out {}`",
        path.display(),
        path.display()
    );
}

#[test]
fn emitted_error_codes_match_from_core_behavior() {
    let rows = error_codes();
    assert!(!rows.is_empty());
    for row in &rows {
        // Reconstruct the representative kind for this row's family and
        // prove the emitted constants are exactly what from_core answers.
        let kind = match row.core_kind {
            "not_found" => ErrorKind::NotFound,
            "conflict" => ErrorKind::Conflict,
            "invalid_state" => ErrorKind::InvalidState {
                from: faktor_core::state::AgentState::Idle,
                to: faktor_core::state::AgentState::Completed,
            },
            "permission" => ErrorKind::Permission,
            "timeout" => ErrorKind::Timeout,
            "cancelled" => ErrorKind::Cancelled,
            "store" => ErrorKind::Store,
            "network" => ErrorKind::Network,
            "provider" => ErrorKind::Provider {
                code: "provider".into(),
                retryable: true,
            },
            "malformed" => ErrorKind::Malformed,
            "oversized" => ErrorKind::Oversized,
            "rate_limited" => ErrorKind::RateLimited,
            "deadlock" => ErrorKind::Deadlock,
            "internal" => ErrorKind::Internal,
            other => panic!("schema row names an unknown core kind {other}"),
        };
        let api = faktor_protocol::error::from_core(&Error::new(kind, "probe"));
        assert_eq!(row.code, api.code, "{}: code drift", row.core_kind);
        assert_eq!(
            row.http_status, api.http_status,
            "{}: status drift",
            row.core_kind
        );
        assert_eq!(
            row.retryable, api.retryable,
            "{}: retryable drift",
            row.core_kind
        );
    }
}

#[test]
fn artifact_declares_its_identity() {
    let value: serde_json::Value = serde_json::from_str(&canonical_json()).unwrap();
    assert_eq!(value["schema"], SCHEMA_ID);
    let types = value["types"].as_array().unwrap();
    let names: Vec<&str> = types.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for required in [
        "Message",
        "Part",
        "ToolResultBody",
        "PageMeta",
        "MessagesPage",
        "SessionState",
        "AgentStateView",
    ] {
        assert!(names.contains(&required), "missing type {required}");
    }
}
