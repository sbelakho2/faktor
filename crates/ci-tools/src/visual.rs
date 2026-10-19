use crate::util::{read_json, Result};
use serde_json::{Map, Value};
use std::path::Path;

const CLASS_RELEASE: &str = "release";
const CLASS_CI_EVIDENCE: &str = "ci-evidence";
const CLASSES: [&str; 2] = [CLASS_RELEASE, CLASS_CI_EVIDENCE];
const BOUNDARY_EXIT: i32 = 3;

fn prefixes(platform: &str) -> Option<&'static [&'static str]> {
    match platform {
        "linux" => Some(&["linux-"]),
        "macos" => Some(&["mac-os-", "darwin-"]),
        "windows" => Some(&["windows-"]),
        _ => None,
    }
}

fn record_for<'a>(document: &'a Value, platform: &str) -> Option<&'a Map<String, Value>> {
    document.get("platforms")?.get(platform)?.as_object()
}

fn fingerprinted(environment: &str) -> bool {
    let Some(index) = environment.rfind("-f") else {
        return false;
    };
    let suffix = &environment[index + 2..];
    suffix.len() >= 8
        && suffix
            .chars()
            .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
}

pub fn check(document: &Value, platform: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(platform_prefixes) = prefixes(platform) else {
        return vec![format!(
            "{platform}=unknown-platform (expected one of linux, macos, windows; a record is only accepted for a release platform)"
        )];
    };
    let Some(record) = record_for(document, platform) else {
        return vec![format!(
            "{platform}=missing (the lane must pin its own record before release)"
        )];
    };
    let environment = record.get("environment").and_then(Value::as_str);
    match environment {
        Some(environment) if fingerprinted(environment) => {}
        _ => problems.push(format!(
            "{platform}=unfingerprinted (environment {environment:?} lacks a resolved -f<hex> font fingerprint)"
        )),
    }
    if let Some(environment) = environment {
        if !platform_prefixes
            .iter()
            .any(|prefix| environment.starts_with(prefix))
        {
            problems.push(format!(
                "{platform}=foreign-environment (environment {environment:?} does not name the {platform} host; prefixes {platform_prefixes:?}; another platform's render is never accepted)"
            ));
        }
    }
    let mut digests = Map::new();
    match record.get("digests").and_then(Value::as_object) {
        Some(value) if !value.is_empty() => digests = value.clone(),
        _ => problems.push(format!("{platform}=digests-missing")),
    }
    for (family, digest) in &digests {
        let valid = digest
            .as_str()
            .map(|text| text.len() >= 16)
            .unwrap_or(false);
        if !valid {
            problems.push(format!("{platform}=digest-invalid:{family}"));
        }
    }
    if let Some(platforms) = document.get("platforms").and_then(Value::as_object) {
        for (other, other_record) in platforms {
            if other == platform || !other_record.is_object() {
                continue;
            }
            if !digests.is_empty()
                && other_record.get("digests") == Some(&Value::Object(digests.clone()))
            {
                problems.push(format!("{platform}=copied-from-{other}"));
            }
        }
    }
    problems
}

pub fn classify(document: &Value, platform: &str, class_name: &str) -> (String, Vec<String>) {
    let problems = check(document, platform);
    if problems.is_empty() {
        return ("certified".into(), problems);
    }
    let missing_record = class_name == CLASS_CI_EVIDENCE
        && prefixes(platform).is_some()
        && document.get("platforms").is_some_and(Value::is_object)
        && record_for(document, platform).is_none()
        && problems
            .iter()
            .all(|problem| problem.starts_with(&format!("{platform}=missing")));
    if missing_record {
        ("not_certified-on-platform".into(), problems)
    } else {
        ("failure".into(), problems)
    }
}

fn selftest() -> i32 {
    let mut failures = 0;
    let mut check_case = |name: &str, ok: bool| {
        if ok {
            println!("selftest ok: {name}");
        } else {
            eprintln!("selftest FAIL: {name}");
            failures += 1;
        }
    };
    let good: Value = serde_json::json!({
        "platforms": {
            "linux": {"environment": "linux-x86_64-jvm17-f0a1b2c3d4e5", "digests": {"a": "a".repeat(64)}},
            "macos": {"environment": "mac-os-x-aarch64-jvm17-f0a1b2c3d4e5", "digests": {"a": "b".repeat(64)}},
            "windows": {"environment": "windows-amd64-jvm17-f0a1b2c3d4e5", "digests": {"a": "c".repeat(64)}},
        }
    });
    check_case(
        "a fingerprinted record passes",
        check(&good, "macos").is_empty(),
    );
    check_case(
        "a missing record refuses",
        check(&serde_json::json!({"platforms": {}}), "macos")
            .iter()
            .any(|problem| problem.contains("missing")),
    );
    check_case(
        "a record without a resolved font fingerprint refuses",
        check(
            &serde_json::json!({"platforms": {"macos": {"environment": "mac-os-x-aarch64-jvm17", "digests": {"a": "b".repeat(64)}}}}),
            "macos",
        )
        .iter()
        .any(|problem| problem.contains("unfingerprinted")),
    );
    check_case(
        "a copied record refuses",
        check(
            &serde_json::json!({"platforms": {
                "linux": {"environment": "linux-f0a1b2c3", "digests": {"a": "a".repeat(64)}},
                "macos": {"environment": "mac-os-f0a1b2c3", "digests": {"a": "a".repeat(64)}},
            }}),
            "macos",
        )
        .iter()
        .any(|problem| problem.contains("copied-from-linux")),
    );
    check_case(
        "a linux render under the macos key refuses (foreign environment)",
        check(
            &serde_json::json!({"platforms": {"macos": {"environment": "linux-amd64-jvm17-f0a1b2c3d4e5", "digests": {"a": "b".repeat(64)}}}}),
            "macos",
        )
        .iter()
        .any(|problem| problem.contains("foreign-environment")),
    );
    check_case(
        "the darwin alias is accepted as this platform's own spelling",
        check(
            &serde_json::json!({"platforms": {"macos": {"environment": "darwin-arm64-jvm17-f0a1b2c3d4e5", "digests": {"a": "b".repeat(64)}}}}),
            "macos",
        )
        .is_empty(),
    );
    check_case(
        "an unknown platform refuses",
        check(&good, "freebsd")
            .iter()
            .any(|problem| problem.contains("unknown-platform")),
    );
    let (kind, _) = classify(
        &serde_json::json!({"platforms": {}}),
        "windows",
        CLASS_CI_EVIDENCE,
    );
    check_case(
        "ci-evidence: a missing platform is a typed boundary",
        kind == "not_certified-on-platform",
    );
    let (kind, _) = classify(
        &serde_json::json!({"platforms": {}}),
        "windows",
        CLASS_RELEASE,
    );
    check_case("release: a missing platform refuses", kind == "failure");
    let (kind, _) = classify(
        &serde_json::json!({"platforms": {"windows": {"environment": "windows-amd64-jvm17", "digests": {"a": "b".repeat(64)}}}}),
        "windows",
        CLASS_CI_EVIDENCE,
    );
    check_case(
        "ci-evidence: an unfingerprinted record is a hard failure, never a boundary",
        kind == "failure",
    );
    let (kind, _) = classify(&good, "windows", CLASS_CI_EVIDENCE);
    check_case(
        "ci-evidence: a certified record stays certified",
        kind == "certified",
    );
    let (kind, _) = classify(
        &serde_json::json!({"schema": "faktor-parity-visual-baselines/v2", "panelDigests": {}}),
        "windows",
        CLASS_CI_EVIDENCE,
    );
    check_case(
        "ci-evidence: a v2 baseline without platform records is not a boundary",
        kind == "failure",
    );
    if failures > 0 {
        eprintln!("visual-baseline selftest: FAIL ({failures})");
        return 1;
    }
    println!("visual-baseline selftest: PASS");
    0
}

fn check_file(platform: &str, path: &str, class_name: &str) -> i32 {
    if !Path::new(path).is_file() {
        eprintln!("visual-baseline: {path} does not exist");
        return 1;
    }
    let document: Result<Value> = read_json(Path::new(path));
    let document = match document {
        Ok(document) => document,
        Err(error) => {
            eprintln!("visual-baseline: {path} is not valid JSON: {error}");
            return 1;
        }
    };
    let (kind, problems) = classify(&document, platform, class_name);
    match kind.as_str() {
        "certified" => {
            println!("visual-baseline: PASS ({platform} record fingerprinted and distinct)");
            0
        }
        "not_certified-on-platform" => {
            println!(
                "visual-baseline: {platform}=not_certified-on-platform (CI-evidence platform boundary: no committed record for this platform; the release class still refuses via scripts/check-visual-platforms.mjs --release)"
            );
            BOUNDARY_EXIT
        }
        _ => {
            for problem in problems {
                eprintln!("visual-baseline: {problem}");
            }
            eprintln!("visual-baseline: {platform} visual proof incomplete");
            1
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("selftest") {
        return selftest();
    }
    let mut platform = String::new();
    let mut path = String::new();
    let mut class_name = CLASS_RELEASE.to_string();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--platform" if index + 1 < args.len() => {
                platform = args[index + 1].clone();
                index += 2;
                continue;
            }
            "--file" if index + 1 < args.len() => {
                path = args[index + 1].clone();
                index += 2;
                continue;
            }
            "--class" if index + 1 < args.len() => {
                class_name = args[index + 1].clone();
                index += 2;
                continue;
            }
            _ => index += 1,
        }
    }
    if platform.is_empty() || path.is_empty() {
        eprintln!(
            "usage: check-visual-baseline --platform NAME --file FILE [--class release|ci-evidence] | selftest"
        );
        return 2;
    }
    if !CLASSES.contains(&class_name.as_str()) {
        eprintln!(
            "usage: --class must be one of {} (got {class_name:?})",
            CLASSES.join(", ")
        );
        return 2;
    }
    check_file(&platform, &path, &class_name)
}
