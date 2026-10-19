use crate::util::{self, Result};
use serde_json::{json, Map, Value};
use std::path::Path;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn digest(text: &str) -> String {
    format!("sha256:{}", util::sha256_hex(text.as_bytes()))
}

fn as_int(value: &str) -> i64 {
    value.parse().unwrap_or(0)
}

fn lines_from(path: &str) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn ci_manifest() -> i32 {
    let artifacts_path = env("CERTIFY_M_ARTIFACTS");
    let mut artifacts = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&artifacts_path) {
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some((sha, path)) = line.split_once('\t') {
                artifacts.push(json!({"path": path, "sha256": sha}));
            }
        }
    }
    artifacts.sort_by(|left, right| {
        util::as_str(left.get("path")).cmp(&util::as_str(right.get("path")))
    });
    let artifact_lines: String = artifacts
        .iter()
        .map(|artifact| {
            format!(
                "{}\t{}\n",
                util::as_str(artifact.get("sha256")),
                util::as_str(artifact.get("path"))
            )
        })
        .collect();
    let artifact_digest = digest(&artifact_lines);
    let problems = lines_from(&env("CERTIFY_M_PROBLEMS"));
    let notes = lines_from(&env("CERTIFY_M_NOTES"));
    let context = json!({
        "name": env("CERTIFY_M_CONTEXT"),
        "event": env("CERTIFY_M_CONTEXT_EVENT"),
        "workflow": env("CERTIFY_M_CONTEXT_WORKFLOW"),
        "class": env("CERTIFY_M_CONTEXT_CLASS"),
        "config_file": env("CERTIFY_M_CONTEXT_CONFIG_FILE"),
        "registry": "immutable",
    });
    let observed = json!({
        "repository": env("CERTIFY_M_REPO"),
        "repository_id": env("CERTIFY_M_REPO_ID"),
        "event": env("CERTIFY_M_OBS_EVENT"),
        "workflow": env("CERTIFY_M_OBS_WORKFLOW"),
        "workflow_state": env("CERTIFY_M_OBS_WORKFLOW_STATE"),
        "pipeline_status": env("CERTIFY_M_OBS_PIPELINE_STATUS"),
        "pipeline_id": env("CERTIFY_M_OBS_PIPELINE_ID"),
        "pipeline_number": env("CERTIFY_M_OBS_PIPELINE_NUMBER"),
        "config_file": env("CERTIFY_M_OBS_CONFIG_FILE"),
        "trusted_volumes": env("CERTIFY_M_OBS_TRUSTED_VOLUMES"),
    });
    let attestation = json!({
        "status": env("CERTIFY_M_ATT_STATUS"),
        "origin_context": env("CERTIFY_M_ATT_ORIGIN"),
        "pipeline_number": env("CERTIFY_M_ATT_PIPELINE"),
        "pipeline_id": env("CERTIFY_M_ATT_PIPELINE_ID"),
        "step_pid": env("CERTIFY_M_ATT_STEP_PID"),
        "signature_identity": env("CERTIFY_M_ATT_IDENTITY"),
        "source_sha": env("CERTIFY_M_ATT_SOURCE"),
        "tree_sha": env("CERTIFY_M_ATT_TREE"),
        "artifacts_matched": env("CERTIFY_M_ATT_MATCHED"),
        "artifacts_attested": env("CERTIFY_M_ATT_ARTIFACTS"),
        "evidence_digest": env("CERTIFY_M_ATT_DIGEST"),
    });
    let release_signature = json!({
        "status": if env("CERTIFY_M_REL_STATUS").is_empty() { "not-run".to_string() } else { env("CERTIFY_M_REL_STATUS") },
        "origin_context": env("CERTIFY_M_REL_ORIGIN"),
        "pipeline_number": env("CERTIFY_M_REL_PIPELINE"),
        "pipeline_id": env("CERTIFY_M_REL_PIPELINE_ID"),
        "step_pid": env("CERTIFY_M_REL_STEP_PID"),
        "signature_identity": env("CERTIFY_M_REL_IDENTITY"),
        "role": if env("CERTIFY_M_REL_ROLE").is_empty() { "release-signing".to_string() } else { env("CERTIFY_M_REL_ROLE") },
        "evidence_digest": env("CERTIFY_M_REL_DIGEST"),
        "artifacts_signed": env("CERTIFY_M_REL_ARTIFACTS"),
    });
    let local_status = if env("CERTIFY_M_LOCAL_STATUS").is_empty() {
        "not-run".to_string()
    } else {
        env("CERTIFY_M_LOCAL_STATUS")
    };
    let required = as_int(&env("CERTIFY_M_RA_REQUIRED"));
    let matched = as_int(&env("CERTIFY_M_RA_MATCHED"));
    let attested = as_int(&env("CERTIFY_M_RA_ATTESTED"));
    let missing: Vec<String> = env("CERTIFY_M_RA_MISSING")
        .split(',')
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect();
    let release_artifacts = json!({
        "status": if env("CERTIFY_M_RA_STATUS").is_empty() { "not-run".to_string() } else { env("CERTIFY_M_RA_STATUS") },
        "required": required,
        "matched": matched,
        "attested": attested,
        "missing": missing,
        "branding": if env("CERTIFY_M_RA_BRANDING").is_empty() { "not-run".to_string() } else { env("CERTIFY_M_RA_BRANDING") },
    });
    let fault_tests_path = env("CERTIFY_M_FAULT_TESTS");
    let mut fault_tests = Vec::new();
    if !fault_tests_path.is_empty() && Path::new(&fault_tests_path).exists() {
        fault_tests = lines_from(&fault_tests_path);
        fault_tests.truncate(200);
    }
    let fault_status = if env("CERTIFY_M_FAULT_STATUS").is_empty() {
        "not-run".to_string()
    } else {
        env("CERTIFY_M_FAULT_STATUS")
    };
    let fault_executed = as_int(&env("CERTIFY_M_FAULT_COUNT"));
    let fault_campaign = json!({
        "status": fault_status,
        "executed": fault_executed,
        "evidence_digest": env("CERTIFY_M_FAULT_DIGEST"),
        "tests": fault_tests,
    });
    let release_status = release_artifacts
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let branding = release_artifacts
        .get("branding")
        .and_then(Value::as_str)
        .unwrap_or("");
    let release_preconditions = json!({
        "required_artifacts": required > 0,
        "matched_equals_required": required > 0 && matched == required,
        "attested_equals_required": required > 0 && attested == required,
        "artifact_set_complete": release_status == "complete",
        "artifact_branding_pass": branding == "pass",
        "fault_campaign_pass": fault_status == "pass",
        "fault_tests_executed": fault_executed > 0,
        "release_signing_role_verified": release_signature.get("status").and_then(Value::as_str) == Some("verified"),
    });
    let release_ready = release_preconditions
        .as_object()
        .is_some_and(|map| map.values().all(|value| value == &Value::Bool(true)));
    let context_class = context.get("class").and_then(Value::as_str).unwrap_or("");
    let attestation_status = attestation
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let release_certificate = problems.is_empty()
        && local_status == "pass"
        && (context_class == "trusted" || attestation_status == "verified")
        && release_ready;
    let certificate_class = if release_certificate {
        "release"
    } else if problems.is_empty() {
        "source"
    } else {
        "none"
    };
    let core = json!({
        "artifacts": artifacts,
        "commit": env("CERTIFY_M_COMMIT"),
        "context": context.clone(),
        "event": util::as_str(observed.get("event")),
        "pipeline_number": util::as_str(observed.get("pipeline_number")),
        "pipeline_status": util::as_str(observed.get("pipeline_status")),
        "repository": util::as_str(observed.get("repository")),
        "run_url": env("CERTIFY_M_RUN_URL"),
        "tree": env("CERTIFY_M_TREE"),
        "workflow_state": util::as_str(observed.get("workflow_state")),
        "attestation": attestation.clone(),
        "release_signature": release_signature.clone(),
        "release_artifacts": release_artifacts.clone(),
        "fault_campaign": fault_campaign.clone(),
    });
    let evidence_digest = digest(&util::canonical_json(&core));
    let mut manifest = Map::new();
    manifest.insert("schema".into(), json!("faktor-release-certification/v2"));
    manifest.insert(
        "status".into(),
        json!(if problems.is_empty() {
            "passed"
        } else {
            "failed"
        }),
    );
    manifest.insert("repository".into(), observed["repository"].clone());
    manifest.insert("repository_id".into(), observed["repository_id"].clone());
    manifest.insert("commit".into(), json!(env("CERTIFY_M_COMMIT")));
    manifest.insert("tree".into(), json!(env("CERTIFY_M_TREE")));
    manifest.insert("context".into(), context.clone());
    manifest.insert("woodpecker_host".into(), json!(env("CERTIFY_M_HOST")));
    manifest.insert(
        "pipeline_number".into(),
        observed["pipeline_number"].clone(),
    );
    manifest.insert("pipeline_id".into(), observed["pipeline_id"].clone());
    manifest.insert(
        "pipeline_status".into(),
        observed["pipeline_status"].clone(),
    );
    manifest.insert("workflow".into(), observed["workflow"].clone());
    manifest.insert("workflow_state".into(), observed["workflow_state"].clone());
    manifest.insert("event".into(), observed["event"].clone());
    manifest.insert(
        "trusted_class".into(),
        json!({"expected": context_class, "observed_volumes": observed["trusted_volumes"]}),
    );
    manifest.insert("run_url".into(), json!(env("CERTIFY_M_RUN_URL")));
    manifest.insert("artifacts".into(), Value::Array(artifacts));
    manifest.insert("artifact_digest".into(), json!(artifact_digest));
    manifest.insert("attestation".into(), attestation);
    manifest.insert("release_signature".into(), release_signature);
    manifest.insert("release_artifacts".into(), release_artifacts);
    manifest.insert("fault_campaign".into(), fault_campaign);
    manifest.insert("release_preconditions".into(), release_preconditions);
    manifest.insert("certificate_class".into(), json!(certificate_class));
    manifest.insert("local_gates".into(), json!(local_status));
    manifest.insert("release_certificate".into(), json!(release_certificate));
    manifest.insert(
        "release_rule".into(),
        json!("release = full local gates pass AND (trusted context verified OR signed build attestation verified) AND the release-signing role signature (faktor-release-signature/v1) verifies AND ReleaseArtifactSet complete+attested (required>0, matched==required, branding pass) AND fault campaign pass with executed>0; no flag weakens this"),
    );
    manifest.insert("observed".into(), observed);
    manifest.insert("evidence_digest".into(), json!(evidence_digest));
    manifest.insert(
        "evidence_policy".into(),
        json!("commit-message claims are not evidence; only the observed CI context and signed attestation at this exact commit certify"),
    );
    manifest.insert("certified_at".into(), json!(util::now_iso()));
    manifest.insert(
        "problems".into(),
        Value::Array(problems.iter().cloned().map(Value::String).collect()),
    );
    manifest.insert(
        "notes".into(),
        Value::Array(notes.iter().cloned().map(Value::String).collect()),
    );
    let out = env("CERTIFY_M_OUT");
    if let Err(error) = util::write_json(Path::new(&out), &Value::Object(manifest.clone())) {
        eprintln!("certify manifest: {error}");
        return 1;
    }
    println!(
        "{}",
        if problems.is_empty() {
            "passed"
        } else {
            "failed"
        }
    );
    println!("{}", if release_certificate { "true" } else { "false" });
    println!("{evidence_digest}");
    0
}

pub fn manifest_problems(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        return 2;
    };
    let Ok(manifest) = util::read_json(Path::new(path)) else {
        return 1;
    };
    if let Some(problems) = manifest.get("problems").and_then(Value::as_array) {
        for problem in problems.iter().filter_map(Value::as_str) {
            eprintln!("  - {problem}");
        }
    }
    0
}

pub fn manifest_digest(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        return 2;
    };
    match util::read_json(Path::new(path)) {
        Ok(manifest) => {
            println!(
                "{}",
                manifest
                    .get("evidence_digest")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
            0
        }
        Err(error) => {
            eprintln!("manifest-digest: {error}");
            1
        }
    }
}

pub fn attestation_count(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        return 2;
    };
    let count = util::read_json(Path::new(path))
        .ok()
        .and_then(|value| value.get("artifacts").cloned())
        .map(|artifacts| artifacts.as_object().map(Map::len).unwrap_or(0))
        .unwrap_or(0);
    println!("{count}");
    0
}

pub fn release_attested_count(args: &[String]) -> i32 {
    if args.len() < 2 {
        return 2;
    }
    let artifacts = util::read_json(Path::new(&args[0]))
        .ok()
        .and_then(|value| value.get("artifacts").cloned())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let lines = std::fs::read_to_string(&args[1]).unwrap_or_default();
    let mut count = 0;
    for line in lines.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 3 {
            continue;
        }
        let (_kind, sha, path) = (parts[0], parts[1], parts[2]);
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        if artifacts.get(name).and_then(Value::as_str) == Some(sha) {
            count += 1;
        }
    }
    println!("{count}");
    0
}

pub fn keys_merge(args: &[String]) -> i32 {
    if args.len() < 2 {
        return 2;
    }
    let out = &args[0];
    let mut identities = Map::new();
    for path in &args[1..] {
        match util::read_json(Path::new(path)) {
            Ok(value) => {
                if let Some(map) = value.get("identities").and_then(Value::as_object) {
                    for (key, value) in map {
                        identities.insert(key.clone(), value.clone());
                    }
                }
            }
            Err(error) => {
                eprintln!("keys-merge: {error}");
                return 1;
            }
        }
    }
    let value = json!({"schema": "faktor-signing-allowlist/v2", "identities": identities});
    match util::write_json_compact(Path::new(out), &value) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("keys-merge: {error}");
            1
        }
    }
}

pub fn strip_signature(args: &[String]) -> i32 {
    if args.len() < 2 {
        return 2;
    }
    match util::read_json(Path::new(&args[0])) {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert("signature".into(), Value::Null);
            }
            match util::write_json_compact(Path::new(&args[1]), &value) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("strip-signature: {error}");
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("strip-signature: {error}");
            1
        }
    }
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

fn text_at<'a>(value: &'a Value, path: &str) -> &'a str {
    util::field(value, path)
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn eq_text(value: &Value, path: &str, expected: &str) -> Result<()> {
    require(
        text_at(value, path) == expected,
        &format!(
            "{path}: expected '{expected}', got '{:?}'",
            util::field(value, path)
        ),
    )
}

fn bool_at(value: &Value, path: &str) -> bool {
    util::field(value, path)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn int_at(value: &Value, path: &str) -> i64 {
    util::field(value, path)
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn has_note(value: &Value, needle: &str) -> bool {
    value
        .get("notes")
        .and_then(Value::as_array)
        .is_some_and(|notes| {
            notes
                .iter()
                .filter_map(Value::as_str)
                .any(|note| note.contains(needle))
        })
}

pub fn manifest_check(args: &[String]) -> i32 {
    if args.len() < 2 {
        return 2;
    }
    let name = &args[0];
    let path = &args[1];
    let manifest = match util::read_json(Path::new(path)) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let result = (|| -> Result<()> {
        match name.as_str() {
            "success" => {
                eq_text(&manifest, "status", "passed")?;
                eq_text(&manifest, "schema", "faktor-release-certification/v2")?;
                eq_text(
                    &manifest,
                    "commit",
                    "0123456789abcdef0123456789abcdef01234567",
                )?;
                eq_text(&manifest, "context.name", "ci/woodpecker/pr/pr")?;
                eq_text(&manifest, "context.class", "untrusted")?;
                eq_text(&manifest, "context.registry", "immutable")?;
                eq_text(&manifest, "event", "pull_request")?;
                eq_text(&manifest, "workflow", "pr")?;
                eq_text(&manifest, "workflow_state", "success")?;
                eq_text(&manifest, "pipeline_id", "111")?;
                eq_text(&manifest, "pipeline_number", "11")?;
                eq_text(&manifest, "trusted_class.expected", "untrusted")?;
                eq_text(&manifest, "trusted_class.observed_volumes", "false")?;
                eq_text(&manifest, "observed.pipeline_id", "111")?;
                eq_text(&manifest, "observed.event", "pull_request")?;
                require(
                    !bool_at(&manifest, "release_certificate"),
                    "release_certificate",
                )?;
                eq_text(&manifest, "attestation.status", "absent")?;
                let digest = text_at(&manifest, "evidence_digest");
                require(
                    digest.starts_with("sha256:") && digest.len() == 71,
                    "evidence_digest",
                )?;
                require(
                    text_at(&manifest, "run_url").ends_with("/repos/7/pipeline/11"),
                    "run_url",
                )?;
                require(
                    manifest
                        .get("artifacts")
                        .and_then(Value::as_array)
                        .is_some_and(|items| items.len() == 3),
                    "artifacts",
                )?;
                eq_text(&manifest, "release_artifacts.status", "complete")?;
                require(
                    int_at(&manifest, "release_artifacts.required") == 3
                        && int_at(&manifest, "release_artifacts.matched") == 3,
                    "release_artifacts matched",
                )?;
                require(
                    int_at(&manifest, "release_artifacts.attested") == 0,
                    "release_artifacts attested",
                )?;
                require(
                    text_at(&manifest, "fault_campaign.status") == "pass"
                        && int_at(&manifest, "fault_campaign.executed") > 0,
                    "fault_campaign",
                )?;
                eq_text(&manifest, "certificate_class", "source")?;
            }
            "trusted-success" => {
                eq_text(&manifest, "status", "passed")?;
                eq_text(&manifest, "context.class", "trusted")?;
                eq_text(&manifest, "trusted_class.expected", "trusted")?;
                eq_text(&manifest, "trusted_class.observed_volumes", "true")?;
                eq_text(&manifest, "event", "push")?;
                eq_text(&manifest, "workflow", "trusted")?;
                eq_text(&manifest, "observed.pipeline_id", "211")?;
                eq_text(&manifest, "attestation.status", "verified")?;
                eq_text(&manifest, "attestation.pipeline_number", "21")?;
                eq_text(
                    &manifest,
                    "attestation.signature_identity",
                    "faktor-selftest",
                )?;
                require(
                    text_at(&manifest, "attestation.evidence_digest").starts_with("sha256:"),
                    "attestation.evidence_digest",
                )?;
                eq_text(&manifest, "release_signature.status", "verified")?;
                eq_text(&manifest, "release_signature.role", "release-signing")?;
                eq_text(
                    &manifest,
                    "release_signature.signature_identity",
                    "faktor-release-selftest",
                )?;
                eq_text(&manifest, "release_signature.pipeline_number", "21")?;
                require(
                    text_at(&manifest, "release_signature.evidence_digest").starts_with("sha256:"),
                    "release_signature.evidence_digest",
                )?;
                require(
                    bool_at(
                        &manifest,
                        "release_preconditions.release_signing_role_verified",
                    ),
                    "release_signing_role_verified",
                )?;
                eq_text(&manifest, "release_artifacts.status", "complete")?;
                require(
                    int_at(&manifest, "release_artifacts.required") == 3
                        && int_at(&manifest, "release_artifacts.attested") == 3,
                    "release_artifacts",
                )?;
                eq_text(&manifest, "release_artifacts.branding", "pass")?;
                require(
                    text_at(&manifest, "fault_campaign.status") == "pass"
                        && int_at(&manifest, "fault_campaign.executed") > 0,
                    "fault_campaign",
                )?;
                eq_text(&manifest, "certificate_class", "release")?;
                require(
                    bool_at(&manifest, "release_certificate"),
                    "release_certificate",
                )?;
            }
            "release-signature-absent" => {
                eq_text(&manifest, "status", "passed")?;
                eq_text(&manifest, "attestation.status", "verified")?;
                eq_text(&manifest, "release_signature.status", "absent")?;
                require(
                    !bool_at(
                        &manifest,
                        "release_preconditions.release_signing_role_verified",
                    ),
                    "release_signing_role_verified",
                )?;
                require(
                    !bool_at(&manifest, "release_certificate"),
                    "release_certificate",
                )?;
                eq_text(&manifest, "certificate_class", "source")?;
                require(
                    has_note(&manifest, "release-signature-absent"),
                    "release-signature-absent note",
                )?;
            }
            "pr-upgraded" => {
                eq_text(&manifest, "context.class", "untrusted")?;
                eq_text(&manifest, "attestation.status", "verified")?;
                eq_text(
                    &manifest,
                    "attestation.origin_context",
                    "ci/woodpecker/push/trusted",
                )?;
                eq_text(&manifest, "release_signature.status", "verified")?;
                eq_text(
                    &manifest,
                    "release_signature.origin_context",
                    "ci/woodpecker/push/trusted",
                )?;
                eq_text(&manifest, "release_signature.role", "release-signing")?;
                require(
                    int_at(&manifest, "release_artifacts.attested") == 3,
                    "release_artifacts.attested",
                )?;
                eq_text(&manifest, "certificate_class", "release")?;
                require(
                    bool_at(&manifest, "release_certificate"),
                    "release_certificate",
                )?;
            }
            "pr-no-keys" => {
                eq_text(&manifest, "attestation.status", "not-checked")?;
                require(
                    !bool_at(&manifest, "release_certificate"),
                    "release_certificate",
                )?;
                require(
                    has_note(&manifest, "attestation-not-checked"),
                    "attestation-not-checked note",
                )?;
            }
            "ci-evidence" => {
                require(
                    !bool_at(&manifest, "release_certificate"),
                    "release_certificate",
                )?;
                eq_text(&manifest, "local_gates", "skipped")?;
            }
            other => return Err(format!("unknown manifest check '{other}'")),
        }
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{name}: {error}");
            1
        }
    }
}

pub fn manifest_has_problem(args: &[String]) -> i32 {
    if args.len() < 2 {
        return 2;
    }
    let manifest = util::read_json(Path::new(&args[0])).ok();
    let hit = manifest
        .as_ref()
        .and_then(|manifest| manifest.get("problems"))
        .and_then(Value::as_array)
        .is_some_and(|problems| {
            problems
                .iter()
                .filter_map(Value::as_str)
                .any(|problem| problem.contains(&args[1]))
        });
    println!("{}", if hit { "yes" } else { "no" });
    0
}
