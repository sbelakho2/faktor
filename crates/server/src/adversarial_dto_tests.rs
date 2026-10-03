//! Adversarial strict-DTO tests (task category 2): unknown/missing/duplicate
//! fields, wrong types, numeric extremes, recursion and duplicate keys for
//! every native request body. The production entry point is the same
//! `serde::Deserialize` impl `axum::Json`/`Query` use on the wire.

use super::*;

/// One strict-DTO battery, each case individually asserted.
macro_rules! strict_dto {
    ($ty:ty, $valid:literal, $empty_ok:expr, $field:literal, $wrong:expr) => {{
        let name = stringify!($ty);
        let valid: serde_json::Value = serde_json::from_str($valid)
            .unwrap_or_else(|e| panic!("{name}: fixture must parse: {e}"));
        assert!(
            serde_json::from_value::<$ty>(valid.clone()).is_ok(),
            "{name}: valid fixture must deserialize"
        );

        let mut unknown = valid.clone();
        unknown["__hostile_unknown__"] = serde_json::json!({"nested": [1, 2, 3]});
        let err = match serde_json::from_value::<$ty>(unknown) {
            Ok(_) => panic!("{name}: unknown field must be refused"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("unknown field"),
            "{name}: unknown-field refusal names the field: {err}"
        );

        let empty = serde_json::from_value::<$ty>(serde_json::json!({}));
        if $empty_ok {
            assert!(empty.is_ok(), "{name}: empty object is fully defaulted");
        } else {
            let err = match empty {
                Ok(_) => panic!("{name}: empty object must be refused"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("missing field"),
                "{name}: missing-field refusal names the field: {err}"
            );
        }

        let mut wrong = valid.clone();
        wrong[$field] = $wrong;
        let err = match serde_json::from_value::<$ty>(wrong) {
            Ok(_) => panic!("{name}: wrong type for {} must be refused", $field),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("invalid type"),
            "{name}: wrong-type refusal for {} is typed: {err}",
            $field
        );
    }};
}

#[test]
fn native_session_dtos_are_strict() {
    strict_dto!(
        NativeCreateSessionRequest,
        r#"{"provider":"fake","model":"m"}"#,
        false,
        "provider",
        serde_json::json!(7)
    );
    strict_dto!(
        NativePromptRequestBody,
        r#"{"session_id":"1","submission_id":"abc-123","prompt":"hi"}"#,
        false,
        "session_id",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeAbortRequest,
        r#"{"session_id":"1"}"#,
        false,
        "session_id",
        serde_json::json!(7)
    );
    strict_dto!(
        NativePermissionReplyRequest,
        r#"{"session_id":"1","permission_id":"p1","decision":"allow"}"#,
        false,
        "session_id",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeSteerBody,
        r#"{"text":"focus"}"#,
        false,
        "text",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeModelBody,
        r#"{"model":"m"}"#,
        false,
        "model",
        serde_json::json!(7)
    );
    strict_dto!(
        NativePresentationBody,
        r#"{"state":"foreground"}"#,
        false,
        "state",
        serde_json::json!(7)
    );
    // The decide body is deliberately EMPTY: any member is unknown.
    assert!(
        serde_json::from_value::<NativeTournamentDecideBody>(serde_json::json!({})).is_ok(),
        "NativeTournamentDecideBody: empty object is the only valid shape"
    );
    let err = match serde_json::from_value::<NativeTournamentDecideBody>(
        serde_json::json!({"reason": "ok"}),
    ) {
        Ok(_) => panic!("NativeTournamentDecideBody: any member must be refused"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("unknown field"),
        "NativeTournamentDecideBody: unknown member refusal: {err}"
    );
    strict_dto!(
        NativeTournamentAbortBody,
        r#"{"reason":"ok"}"#,
        true,
        "reason",
        serde_json::json!(7)
    );
}

#[test]
fn native_query_dtos_are_strict() {
    strict_dto!(
        NativeMessagesQuery,
        r#"{"session":"1"}"#,
        false,
        "session",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeEventsQuery,
        r#"{"session":"1"}"#,
        false,
        "session",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeAgentsQuery,
        r#"{"session":"1"}"#,
        false,
        "session",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeOrchestratorGraphQuery,
        r#"{"session":"1"}"#,
        false,
        "session",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeEvidenceQuery,
        r#"{"session":"1"}"#,
        false,
        "session",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeTerminalsQuery,
        r#"{"session":"1"}"#,
        false,
        "session",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeSessionEventsQuery,
        r#"{}"#,
        true,
        "after",
        serde_json::json!("x")
    );
    strict_dto!(
        NativeTerminalEventsQuery,
        r#"{}"#,
        true,
        "after",
        serde_json::json!("x")
    );
    strict_dto!(
        NativePermissionsQuery,
        r#"{}"#,
        true,
        "session",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeBoardQuery,
        r#"{}"#,
        true,
        "limit",
        serde_json::json!("x")
    );
    strict_dto!(
        NativeUsageQuery,
        r#"{}"#,
        true,
        "limit",
        serde_json::json!("x")
    );
    strict_dto!(WorkersQuery, r#"{}"#, true, "limit", serde_json::json!("x"));
    strict_dto!(PageQuery, r#"{}"#, true, "limit", serde_json::json!("x"));
    strict_dto!(CursorQuery, r#"{}"#, true, "limit", serde_json::json!("x"));
    strict_dto!(
        ApprovalsQuery,
        r#"{}"#,
        true,
        "limit",
        serde_json::json!("x")
    );
}

#[test]
fn terminal_dtos_are_strict() {
    strict_dto!(
        NativeTerminalSpawnBody,
        r#"{"command":"sh","rows":24,"cols":80}"#,
        false,
        "command",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeTerminalInputBody,
        r#"{"data":"ls\n"}"#,
        false,
        "data",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeTerminalResizeBody,
        r#"{"rows":24,"cols":80}"#,
        false,
        "rows",
        serde_json::json!("24")
    );
    strict_dto!(
        NativeTerminalKillBody,
        r#"{"reason":"done"}"#,
        true,
        "reason",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeTerminalReconcileBody,
        r#"{"disposition":"collected"}"#,
        false,
        "disposition",
        serde_json::json!(7)
    );
}

#[test]
fn board_and_task_dtos_are_strict() {
    strict_dto!(
        NativeBoardPostBody,
        r#"{"subject":"s","body":"b","refs":["a"]}"#,
        false,
        "subject",
        serde_json::json!(7)
    );
    strict_dto!(
        StartTaskRunRequest,
        r#"{"goal":"g","submission_id":"abc-123"}"#,
        false,
        "goal",
        serde_json::json!(7)
    );
    strict_dto!(
        StartTournamentRequest,
        r#"{"goal":"g","criteria":[],"n":2}"#,
        false,
        "goal",
        serde_json::json!(7)
    );
    strict_dto!(
        NativeBudgetBody,
        r#"{"max_tokens":10,"max_cost_micro":5}"#,
        true,
        "max_tokens",
        serde_json::json!("ten")
    );
}

#[test]
fn control_plane_dtos_are_strict() {
    strict_dto!(
        BootstrapOrganizationBody,
        r#"{"name":"N","owner_email":"a@b.test","display_name":"D"}"#,
        false,
        "name",
        serde_json::json!(7)
    );
    strict_dto!(
        InviteMemberBody,
        r#"{"email":"a@b.test","role":"member"}"#,
        false,
        "email",
        serde_json::json!(7)
    );
    strict_dto!(
        CreateApprovalBody,
        r#"{"action":"merge","resource":"repo","reason":"why"}"#,
        false,
        "action",
        serde_json::json!(7)
    );
    strict_dto!(
        DecideApprovalBody,
        r#"{"approved":true,"note":"ok"}"#,
        false,
        "approved",
        serde_json::json!("yes")
    );
    strict_dto!(
        CreditGrantBody,
        r#"{"account_id":"acct","amount_micro":1,"reason":"r"}"#,
        false,
        "account_id",
        serde_json::json!(7)
    );
    strict_dto!(
        SsoStartBody,
        r#"{"organization":"org","redirect_uri":"https://x.test/cb"}"#,
        false,
        "organization",
        serde_json::json!(7)
    );
    strict_dto!(
        SsoCallbackBody,
        r#"{"organization":"org","redirect_uri":"https://x.test/cb","code":"c","state":"s"}"#,
        false,
        "organization",
        serde_json::json!(7)
    );
    strict_dto!(
        SsoLogoutBody,
        r#"{"organization":"org","session_id":"1"}"#,
        false,
        "organization",
        serde_json::json!(7)
    );
    strict_dto!(
        MintTokenBody,
        r#"{"label":"l","trust_domain":"td"}"#,
        true,
        "label",
        serde_json::json!(7)
    );
    strict_dto!(
        ClaimBody,
        r#"{"token":"tok","protocol_version":1}"#,
        false,
        "token",
        serde_json::json!(7)
    );
    strict_dto!(
        HeartbeatBody,
        r#"{"token":"tok","protocol_version":1,"lease_id":"l","generation":0}"#,
        false,
        "token",
        serde_json::json!(7)
    );
    strict_dto!(
        RegisterArtifactBody,
        r#"{"id":"a1","session":"1","task":1,"owner":"o","kind":"k","digest":"sha256:aa","size":1,"retention_class":"r"}"#,
        false,
        "id",
        serde_json::json!(7)
    );
}

#[test]
fn numeric_extremes_are_typed_per_bound_field() {
    // (json, ok) tables per numeric field. `-1`/floats/strings must be
    // refused; the u64 ceiling is accepted by the deserializer and bounded
    // later by the handler's page-limit logic.
    let limit_cases: Vec<(serde_json::Value, bool, &str)> = vec![
        (serde_json::json!(0), true, "zero"),
        (serde_json::json!(1), true, "one"),
        (serde_json::json!(-1), false, "negative"),
        (
            serde_json::json!(-9223372036854775808i64),
            false,
            "i64::MIN",
        ),
        (serde_json::json!(1.5), false, "float"),
        (serde_json::json!("5"), false, "string"),
        (serde_json::json!(null), true, "null"),
        (serde_json::json!(9223372036854775807i64), true, "i64::MAX"),
        (serde_json::json!(u64::MAX), true, "u64::MAX"),
    ];
    for (value, ok, label) in &limit_cases {
        let mut body = serde_json::json!({"session": "1"});
        body["limit"] = value.clone();
        let result = serde_json::from_value::<NativeMessagesQuery>(body);
        assert_eq!(
            result.is_ok(),
            *ok,
            "NativeMessagesQuery.limit {label}: {value}"
        );
    }
    for (value, ok, label) in limit_cases {
        let mut body = serde_json::json!({});
        body["limit"] = value.clone();
        let result = serde_json::from_value::<NativeUsageQuery>(body);
        assert_eq!(
            result.is_ok(),
            ok,
            "NativeUsageQuery.limit {label}: {value}"
        );
    }
    // u32 fields (terminal resize) refuse negatives and > u32::MAX.
    let resize_cases: Vec<(serde_json::Value, bool, &str)> = vec![
        (serde_json::json!(0), true, "zero"),
        (serde_json::json!(1), true, "one"),
        (serde_json::json!(-1), false, "negative"),
        (serde_json::json!(4294967295u64), true, "u32::MAX"),
        (serde_json::json!(4294967296u64), false, "u32::MAX+1"),
        (serde_json::json!(u64::MAX), false, "u64::MAX"),
        (serde_json::json!(1.5), false, "float"),
    ];
    for (value, ok, label) in resize_cases {
        let body = serde_json::json!({"rows": value, "cols": 80});
        let result = serde_json::from_value::<NativeTerminalResizeBody>(body);
        assert_eq!(
            result.is_ok(),
            ok,
            "NativeTerminalResizeBody.rows {label}: {value}"
        );
    }
    // u64 option field: u64::MAX parses but must not overflow downstream.
    for (value, ok, label) in [
        (serde_json::json!(0), true, "zero"),
        (serde_json::json!(u64::MAX), true, "u64::MAX"),
        (serde_json::json!(-1), false, "negative"),
        (serde_json::json!(1e300), false, "huge float"),
    ] {
        let body = serde_json::json!({"max_tokens": value});
        let result = serde_json::from_value::<NativeBudgetBody>(body);
        assert_eq!(
            result.is_ok(),
            ok,
            "NativeBudgetBody.max_tokens {label}: {value}"
        );
    }
    for (value, ok, label) in [
        (serde_json::json!(u64::MAX), true, "u64::MAX"),
        (serde_json::json!(-1), false, "negative"),
        (serde_json::json!(1.25), false, "float"),
        (serde_json::json!("123"), true, "decimal string"),
        (
            serde_json::json!("not-a-number"),
            false,
            "bad decimal string",
        ),
        (serde_json::json!(""), false, "empty decimal string"),
    ] {
        let body = serde_json::json!({"max_cost_micro": value});
        let result = serde_json::from_value::<NativeBudgetBody>(body);
        assert_eq!(
            result.is_ok(),
            ok,
            "NativeBudgetBody.max_cost_micro {label}: {value}"
        );
    }
    // before is an i64 cursor: the extremes are typed.
    for (value, ok, label) in [
        (serde_json::json!(0), true, "zero"),
        (serde_json::json!(i64::MAX), true, "i64::MAX"),
        (serde_json::json!(i64::MIN), true, "i64::MIN"),
        (serde_json::json!(u64::MAX), false, "u64::MAX overflows i64"),
        (serde_json::json!(1.5), false, "float"),
    ] {
        let body = serde_json::json!({"session": "1", "before": value});
        let result = serde_json::from_value::<NativeMessagesQuery>(body);
        assert_eq!(
            result.is_ok(),
            ok,
            "NativeMessagesQuery.before {label}: {value}"
        );
    }
}

#[test]
fn duplicate_json_keys_are_refused_by_the_derive() {
    // serde's derive rejects duplicate fields on the raw-text path (the
    // path `axum::Json` uses against the request bytes).
    let cases: Vec<(&str, &str)> = vec![
        (
            "NativeCreateSessionRequest",
            r#"{"provider":"a","provider":"b","model":"m"}"#,
        ),
        (
            "NativePromptRequestBody",
            r#"{"session_id":"1","session_id":"2","submission_id":"abc-123","prompt":"x"}"#,
        ),
        ("NativeTerminalInputBody", r#"{"data":"a","data":"b"}"#),
        (
            "SsoStartBody",
            r#"{"organization":"o","organization":"p","redirect_uri":"https://x.test"}"#,
        ),
        (
            "DecideApprovalBody",
            r#"{"approved":true,"approved":false,"note":"n"}"#,
        ),
    ];
    for (name, raw) in cases {
        let outcome: Result<(), String> = match name {
            "NativeCreateSessionRequest" => {
                serde_json::from_str::<NativeCreateSessionRequest>(raw).map(|_| ())
            }
            "NativePromptRequestBody" => {
                serde_json::from_str::<NativePromptRequestBody>(raw).map(|_| ())
            }
            "NativeTerminalInputBody" => {
                serde_json::from_str::<NativeTerminalInputBody>(raw).map(|_| ())
            }
            "SsoStartBody" => serde_json::from_str::<SsoStartBody>(raw).map(|_| ()),
            "DecideApprovalBody" => serde_json::from_str::<DecideApprovalBody>(raw).map(|_| ()),
            other => panic!("unknown fixture {other}"),
        }
        .map_err(|e| e.to_string());
        let err = outcome.expect_err(&format!("{name}: duplicate keys must be refused"));
        assert!(
            err.contains("duplicate field") || err.contains("duplicate"),
            "{name}: duplicate-key refusal is typed: {err}"
        );
    }
    // Duplicate keys with DIFFERENT values never silently take one value.
    let raw = r#"{"provider":"a","provider":"b","model":"m"}"#;
    let err = serde_json::from_str::<NativeCreateSessionRequest>(raw)
        .expect_err("duplicate provider must be refused");
    assert!(
        !format!("{err}").contains("b"),
        "the refusal must not echo a chosen duplicate value: {err}"
    );
}

#[test]
fn deep_nesting_is_bounded_by_the_json_parser() {
    let depth = 200;
    let mut deep = String::new();
    for _ in 0..depth {
        deep.push('[');
    }
    for _ in 0..depth {
        deep.push(']');
    }
    let err = serde_json::from_str::<serde_json::Value>(&deep)
        .expect_err("a 200-deep array must hit the recursion bound");
    assert!(
        err.to_string().contains("recursion limit"),
        "the recursion refusal is typed: {err}"
    );
    // A wrapper object with a deep value is refused the same way.
    let wrapped = format!("{{\"data\": {deep}}}");
    let err = match serde_json::from_str::<NativeTerminalInputBody>(&wrapped) {
        Ok(_) => panic!("a deep value inside a DTO must be refused"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("recursion limit") || err.to_string().contains("invalid type"),
        "wrapped deep-value refusal is typed: {err}"
    );
    // Depth just under the bound parses (the parser is not off-by-one
    // hostile in the safe direction).
    let shallow = "[[[[1]]]]".to_string();
    assert!(
        serde_json::from_str::<serde_json::Value>(&shallow).is_ok(),
        "shallow nesting must parse"
    );
}

#[test]
fn proof_query_scope_is_closed_and_typed() {
    // `NativeTaskScopeQuery` is a strict QUERY DTO like its siblings: an
    // unknown query parameter is refused, never silently ignored, and the
    // session value is bounded downstream.
    assert!(
        serde_json::from_value::<NativeTaskScopeQuery>(
            serde_json::json!({"session": "1", "extra": true})
        )
        .is_err(),
        "unknown query params are refused"
    );
    assert!(
        serde_json::from_value::<NativeTaskScopeQuery>(serde_json::json!({})).is_ok(),
        "absent scope is null"
    );
    let err =
        match serde_json::from_value::<NativeTaskScopeQuery>(serde_json::json!({"session": 7})) {
            Ok(_) => panic!("wrong session type must be refused"),
            Err(e) => e,
        };
    assert!(
        err.to_string().contains("invalid type"),
        "scope type refusal: {err}"
    );
}
