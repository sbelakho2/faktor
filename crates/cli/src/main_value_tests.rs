//! `main_value_tests`: the value-centric entitlement doctor section
//! (inference-independent paid value features, the offline default and the
//! plan-tier mapping).

use super::*;

fn write_config(dir: &std::path::Path, body: serde_json::Value) -> std::path::PathBuf {
    let path = dir.join("faktor-plus.json");
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

/// The offline default: with no config (and with an explicitly disabled
/// billing section) doctor reports disabled, denies typed and raises NO
/// issue — the local product is unaffected.
#[test]
fn value_entitlements_doctor_reports_the_offline_default_without_config() {
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_value_entitlements_line(None, &mut lines, &mut issues);
    assert_eq!(
        issues, 0,
        "the offline default is information, not an issue"
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("value entitlements: state=disabled"));
    assert!(lines[0].contains("offline default"));
    assert!(lines[0].contains("local product is unaffected"));

    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), serde_json::json!({}));
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_value_entitlements_line(Some(&path), &mut lines, &mut issues);
    assert_eq!(issues, 0);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("billing disabled"), "{}", lines[0]);
}

/// The enabled value-only tier fixture (empty `managed_providers`, no
/// limits): doctor maps every plan's value entitlements and effective
/// governance flags — a 100% local customer's paid surface, without any
/// inference volume.
#[test]
fn value_entitlements_doctor_maps_the_tier_fixture() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/entitlements/value-plan-tiers.json"
    ))
    .expect("the value tier fixture");
    let fixture: faktor_cloud::BillingConfig =
        serde_json::from_str(&raw).expect("the fixture is a strict BillingConfig");
    assert!(fixture.managed_providers.is_empty());
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        dir.path(),
        serde_json::json!({
            "cloud": { "enabled": true },
            "billing": {
                "enabled": true,
                "organization": "org_1",
                "default_plan": "local",
                "plans": fixture.plans,
            }
        }),
    );
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_value_entitlements_line(Some(&path), &mut lines, &mut issues);
    assert_eq!(issues, 0, "{lines:?}");
    assert!(
        lines[0]
            .contains("value entitlements: state=enabled enabled=true plans=4 default_plan=local"),
        "{}",
        lines[0]
    );
    assert!(
        lines.iter().any(|l| l.contains(
            "value plan local: entitlements=advanced_verification_policy,organization_contracts,signed_proof_provenance governance=-"
        )),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("value plan enterprise:") && l.contains("governance=sso,scim")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("value plan pro:") && l.contains("shared_intelligence")),
        "{lines:?}"
    );
    // The full doctor run (store + probes + this section) passes.
    let report = doctor_run_with_config(dir.path(), false, Some(&path));
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("value plan team:") && l.contains("private_runners")),
        "{:?}",
        report.lines
    );
}

/// A config the daemon refuses is reported as refused by this section too —
/// WITHOUT a second issue count (the worker-plane section already counts the
/// refusal once), so doctor fails exactly once per bad config.
#[test]
fn value_entitlements_doctor_reports_a_refused_config_without_double_counting() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        dir.path(),
        serde_json::json!({
            "cloud": { "enabled": true },
            "billing": {
                "enabled": true,
                "organization": "org_1",
                "plans": {
                    "gold": {
                        "plan_id": "gold",
                        "features": ["value_gold"],
                        "limits": {}
                    }
                }
            }
        }),
    );
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_value_entitlements_line(Some(&path), &mut lines, &mut issues);
    assert_eq!(issues, 0, "the wholesale refusal is counted once elsewhere");
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("value entitlements: unavailable state=refused"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("value_gold"), "{}", lines[0]);
}
