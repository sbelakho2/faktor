//! Unmodified upstream-client compatibility: vendored `@kilocode/sdk@7.5.6`
//! fixtures and the live replay test.
//!
//! What is vendored: `packages/sdk/js` from the pinned upstream commit
//! (repo `Kilo-Org/kilocode`, tag `v7.5.6`, commit `fa02955b…`), verbatim
//! under `compat/kilo-v756/upstream-sdk/`, with every file hashed (blake3)
//! in `compat/kilo-v756/upstream.json` and the MIT license text next to it.
//! `compat/kilo-v756/vendor-sdk.sh` documents the fetch protocol.
//!
//! What is tested:
//!
//! 1. **Manifest** (`upstream_manifest_pins_and_hashes_the_vendored_sdk`):
//!    offline; the vendored tree is byte-identical to the pinned upstream
//!    commit (regeneration is deliberate:
//!    `FAKTOR_COMPAT_REGEN_SDK_MANIFEST=1`).
//!
//! 2. **Trace corpus** (`sdk_trace_corpus_is_well_formed`): every
//!    `compat/kilo-v756/sdk-traces/*.json` step is typed (`passing` vs
//!    `divergence`), carries its vendored-type provenance, and `passing`
//!    steps carry a recorded request+response.
//!
//! 3. **Live replay** (`unmodified_upstream_client_replays_against_the_real_daemon`):
//!    spawns a REAL daemon (same harness as the rest of this crate) and runs
//!    the UNMODIFIED client (`tests/compat/js/run.mjs` imports the vendored
//!    `src/v2/client.ts` through a harness-side resolver hook, records every
//!    request from inside the client's own `fetch` config, and replays it
//!    over the wire). Request bytes (method, path+query encoding, body,
//!    content-type, Basic auth header) are compared byte-for-byte against the
//!    checked-in goldens. Responses are compared with the recorded golden:
//!    for `passing` surfaces an exact structural match; for `divergence`
//!    surfaces the recorded daemon behavior is locked AND the documented
//!    missing-SDK-field lock must still hold — if the daemon ever satisfies
//!    the SDK shape, the test fails and forces promotion to `passing` +
//!    docs update.
//!
//!    Node is a harness dependency of the upstream TypeScript client. When
//!    `node` is absent the test records the REQUIRED report as failed (and
//!    panics) under `FAKTOR_COMPAT_REQUIRE_NODE=1`, and otherwise records an
//!    explicit skip and returns. Either way it writes the certification
//!    report consumed by `scripts/capabilities-manifest.mjs`:
//!
//!    ```json
//!    {"schema": "faktor-kilo-compat/v1", "commit": "<repo HEAD>",
//!     "sdk_version": "7.5.6",
//!     "requests": {"passed": N, "total": N},
//!     "responses": {"passed": N, "total": N},
//!     "required_divergences": N,
//!     "status": "passed|partial|failed|skipped"}
//!    ```
//!
//!    at `target/certification/kilo-compat.json`. `status` is `passed` only
//!    when every corpus step is an exact pass (zero required divergences);
//!    `partial` records the measured ratio while documented divergences
//!    remain. Regeneration of the recorded request/response goldens:
//!    `FAKTOR_COMPAT_FREEZE_TRACES=1`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map, Value};

const REPORT_SCHEMA: &str = "faktor-kilo-compat/v1";
const SDK_VERSION: &str = "7.5.6";

const REPOSITORY: &str = "https://github.com/Kilo-Org/kilocode";
const TAG: &str = "v7.5.6";
const COMMIT: &str = "fa02955bfa17b60e57e0d7406d200a73337472ee";
const SDK_UPSTREAM_PATH: &str = "packages/sdk/js";
const VENDOR_DIR: &str = "upstream-sdk";
const HASH_ALGORITHM: &str = "blake3";
const LICENSE_FILE: &str = "LICENSES/kilocode-LICENSE.txt";
const FETCH: &str = concat!(
    "git clone --filter=blob:none --no-checkout --depth 1 --branch v7.5.6 ",
    "https://github.com/Kilo-Org/kilocode <tmp>/repo && ",
    "git -C <tmp>/repo sparse-checkout set packages/sdk/js && ",
    "git -C <tmp>/repo checkout fa02955bfa17b60e57e0d7406d200a73337472ee && ",
    "rsync -a --delete <tmp>/repo/packages/sdk/js/ compat/kilo-v756/upstream-sdk/ && ",
    "cp <tmp>/repo/LICENSE compat/kilo-v756/LICENSES/kilocode-LICENSE.txt",
);
const TRACE_SCHEMA: &str = "faktor.compat.sdk-traces/v1";

fn compat_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/kilo-v756")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The certification report path (`target/certification/kilo-compat.json`
/// under the workspace root — CI runs `cargo test` from that root, so the
/// capabilities manifest and the Woodpecker lane consume the same file).
fn report_path() -> PathBuf {
    workspace_root().join("target/certification/kilo-compat.json")
}

/// The repository HEAD the replay ran on (`unknown` without git metadata;
/// the manifest treats a non-HEAD report as stale and stays PARTIAL).
fn repo_head_commit() -> String {
    Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Every step id → its corpus status (`passing`/`divergence`), read offline.
fn corpus_step_statuses() -> BTreeMap<String, String> {
    let mut statuses = BTreeMap::new();
    for file in trace_files() {
        let group = load_json(&file);
        for step in group["steps"].as_array().unwrap() {
            statuses.insert(
                step["id"].as_str().unwrap().to_string(),
                step["status"].as_str().unwrap().to_string(),
            );
        }
    }
    statuses
}

/// `faktor-kilo-compat/v1` report, written on Drop so a mid-replay panic
/// still leaves `status: failed` evidence with the counts measured so far.
/// A run with documented divergences left records `partial`; only a corpus
/// whose every step is an exact pass records `passed`.
struct KiloCompatReport {
    path: PathBuf,
    commit: String,
    total: usize,
    required_divergences: usize,
    requests_passed: usize,
    responses_passed: usize,
    status: &'static str,
}

impl Drop for KiloCompatReport {
    fn drop(&mut self) {
        let body = json!({
            "schema": REPORT_SCHEMA,
            "commit": self.commit,
            "sdk_version": SDK_VERSION,
            "requests": { "passed": self.requests_passed, "total": self.total },
            "responses": { "passed": self.responses_passed, "total": self.total },
            "required_divergences": self.required_divergences,
            "status": self.status,
        });
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&body) {
            let _ = std::fs::write(&self.path, format!("{text}\n"));
        }
    }
}

fn new_report() -> KiloCompatReport {
    let statuses = corpus_step_statuses();
    KiloCompatReport {
        path: report_path(),
        commit: repo_head_commit(),
        total: statuses.len(),
        required_divergences: statuses.values().filter(|s| *s == "divergence").count(),
        requests_passed: 0,
        responses_passed: 0,
        status: "failed",
    }
}

fn manifest_path() -> PathBuf {
    compat_root().join("upstream.json")
}

fn vendor_root() -> PathBuf {
    compat_root().join(VENDOR_DIR)
}

fn traces_root() -> PathBuf {
    compat_root().join("sdk-traces")
}

fn js_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("js")
}

fn blake3_hex(path: &Path) -> String {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("vendored file {} unreadable: {e}", path.display()));
    blake3::hash(&bytes).to_hex().to_string()
}

/// Recursive files under `root` keyed by POSIX-style relative path.
fn walk_files(root: &Path, prefix: &str, out: &mut BTreeMap<String, PathBuf>) {
    let entries = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("vendored tree {} unreadable: {e}", root.display()));
    for entry in entries {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            walk_files(&entry.path(), &format!("{prefix}{name}/"), out);
        } else if file_type.is_file() {
            out.insert(format!("{prefix}{name}"), entry.path());
        } else {
            panic!("vendored tree contains a non-file at {prefix}{name}");
        }
    }
}

fn manifest_files() -> BTreeMap<String, PathBuf> {
    let mut files = BTreeMap::new();
    walk_files(&vendor_root(), "", &mut files);
    files
}

fn build_manifest() -> Value {
    let files = manifest_files();
    let mut hashes = Map::new();
    let mut total_bytes = 0u64;
    for (rel, abs) in &files {
        let bytes = std::fs::read(abs).unwrap();
        total_bytes += bytes.len() as u64;
        hashes.insert(
            format!("{VENDOR_DIR}/{rel}"),
            json!(blake3::hash(&bytes).to_hex().to_string()),
        );
    }
    let license = compat_root().join(LICENSE_FILE);
    let license_text = std::fs::read_to_string(&license)
        .unwrap_or_else(|e| panic!("license {} unreadable: {e}", license.display()));
    assert!(
        license_text.contains("MIT License"),
        "vendored license must be the MIT text of the pinned commit"
    );
    json!({
        "schema": "faktor.compat.upstream-sdk/v1",
        "repository": REPOSITORY,
        "tag": TAG,
        "commit": COMMIT,
        "paths": { SDK_UPSTREAM_PATH: format!("compat/kilo-v756/{VENDOR_DIR}") },
        "fetch": FETCH,
        "hashAlgorithm": HASH_ALGORITHM,
        "fileCount": files.len(),
        "totalBytes": total_bytes,
        "licenses": {
            "package": "MIT",
            "vendored_from": "LICENSE at the pinned repo root",
            "file_hashes": { LICENSE_FILE: blake3_hex(&license) },
        },
        "file_hashes": hashes,
    })
}

#[test]
fn upstream_manifest_pins_and_hashes_the_vendored_sdk() {
    let regen = std::env::var("FAKTOR_COMPAT_REGEN_SDK_MANIFEST").as_deref() == Ok("1");
    let path = manifest_path();
    if regen || !path.exists() {
        let manifest = build_manifest();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&manifest).unwrap() + "\n",
        )
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
        if !regen {
            panic!(
                "upstream manifest {} was missing; wrote it from the vendored tree — re-run to verify",
                path.display()
            );
        }
    }
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("manifest unreadable: {e}")),
    )
    .unwrap_or_else(|e| panic!("manifest is not valid JSON: {e}"));
    assert_eq!(manifest["repository"], REPOSITORY);
    assert_eq!(manifest["tag"], TAG);
    assert_eq!(manifest["commit"], COMMIT);
    assert_eq!(manifest["hashAlgorithm"], HASH_ALGORITHM);
    assert_eq!(
        manifest["paths"][SDK_UPSTREAM_PATH],
        format!("compat/kilo-v756/{VENDOR_DIR}")
    );

    let files = manifest_files();
    assert_eq!(
        manifest["fileCount"].as_u64().unwrap() as usize,
        files.len(),
        "vendored file count drifted from the pin"
    );
    let total: u64 = files
        .values()
        .map(|p| std::fs::metadata(p).unwrap().len())
        .sum();
    assert_eq!(
        manifest["totalBytes"].as_u64().unwrap(),
        total,
        "vendored byte count drifted from the pin"
    );

    let hashes = manifest["file_hashes"].as_object().unwrap();
    assert_eq!(
        hashes.len(),
        files.len(),
        "manifest hash set size != vendored file count (tamper or stale manifest)"
    );
    for (rel, abs) in &files {
        let key = format!("{VENDOR_DIR}/{rel}");
        let expected = hashes.get(&key).unwrap_or_else(|| {
            panic!("vendored file {key} is not in the pin manifest (tampered tree?)")
        });
        assert_eq!(
            &blake3_hex(abs),
            expected.as_str().unwrap(),
            "hash drift for {key}: the vendored tree diverged from {COMMIT}"
        );
    }
    for key in hashes.keys() {
        assert!(
            files.contains_key(key.strip_prefix(&format!("{VENDOR_DIR}/")).unwrap()),
            "manifest lists {key}, which is missing from the vendored tree"
        );
    }
    let license = compat_root().join(LICENSE_FILE);
    assert_eq!(
        blake3_hex(&license),
        manifest["licenses"]["file_hashes"][LICENSE_FILE]
            .as_str()
            .unwrap()
    );
}

fn trace_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(traces_root())
        .unwrap_or_else(|e| panic!("sdk trace dir {} missing: {e}", traces_root().display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    files.sort();
    files
}

fn load_json(path: &Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("fixture {} unreadable: {e}", path.display())),
    )
    .unwrap_or_else(|e| panic!("fixture {} is not valid JSON: {e}", path.display()))
}

#[test]
fn sdk_trace_corpus_is_well_formed() {
    let files = trace_files();
    assert!(
        files.len() >= 5,
        "sdk trace corpus must keep every surface group: {}",
        files.len()
    );
    let mut ids = BTreeSet::new();
    let mut surfaces = BTreeSet::new();
    for file in &files {
        let group = load_json(file);
        assert_eq!(group["schema"], TRACE_SCHEMA, "{}", file.display());
        assert_eq!(group["sdk"]["commit"], COMMIT, "{}", file.display());
        assert_eq!(group["sdk"]["version"], "7.5.6", "{}", file.display());
        for step in group["steps"].as_array().unwrap() {
            let id = step["id"]
                .as_str()
                .unwrap_or_else(|| panic!("{}: step without id", file.display()));
            assert!(ids.insert(id.to_string()), "duplicate trace id {id}");
            let status = step["status"].as_str().unwrap_or("");
            assert!(
                status == "passing" || status == "divergence",
                "{id}: status must be passing|divergence, got {status:?}"
            );
            assert!(step["call"].is_string(), "{id}: call missing");
            assert!(
                step["sdk_type"].is_string(),
                "{id}: sdk_type provenance missing"
            );
            assert!(step["note"].is_string(), "{id}: honest note missing");
            surfaces.insert(id.split('.').next().unwrap().to_string());
            if status == "passing" {
                assert!(
                    step["request"].is_object() && step["response"].is_object(),
                    "{id}: passing surfaces must carry recorded request+response goldens"
                );
            }
        }
    }
    for expected in [
        "auth",
        "global",
        "session",
        "config",
        "provider",
        "pty",
        "permission",
        "question",
        "network",
        "instance",
    ] {
        assert!(
            surfaces.contains(expected),
            "trace corpus lost the {expected} surface"
        );
    }
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Replace whole JSON strings equal to a captured id with `@string`, and
/// millisecond timestamps with `@int`, so recorded goldens are run-stable.
fn normalize_json(value: &Value, vars: &BTreeMap<String, String>) -> Value {
    match value {
        Value::String(s) => {
            if vars.values().any(|v| v == s)
                || (!s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
            {
                json!("@string")
            } else {
                value.clone()
            }
        }
        Value::Number(n) => {
            if n.as_u64().map(|v| v >= 1_000_000_000_000).unwrap_or(false) {
                json!("@int")
            } else {
                value.clone()
            }
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| normalize_json(v, vars)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), normalize_json(v, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Value-aware path normalization: exact path segments equal to a captured
/// id become `@string`; nothing else changes.
fn normalize_path(value: &str, vars: &BTreeMap<String, String>) -> String {
    value
        .split('/')
        .map(|seg| {
            if vars.values().any(|v| v == seg) {
                "@string".to_string()
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Query normalization: a value equal to a captured id that was actually
/// used by the step becomes `@string` (small ids must not corrupt `limit`).
fn normalize_query(value: &str, vars: &BTreeMap<String, String>) -> String {
    value
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, query_value)) if vars.values().any(|v| v == query_value) => {
                format!("{name}=@string")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn substitute_vars_in_body(text: &str, vars: &BTreeMap<String, String>) -> String {
    // Byte-exact bodies are kept; only whole-JSON-string occurrences of a
    // captured id are templated (never substring replacement).
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return text.to_string(),
    };
    serde_json::to_string(&normalize_json(&parsed, vars)).unwrap()
}

fn template_matches(expected: &Value, actual: &Value, at: &str) -> Result<(), String> {
    if let Value::String(template) = expected {
        if let Some(kind) = template.strip_prefix('@') {
            return match kind {
                "string" => match actual.as_str() {
                    Some(s) if !s.is_empty() => Ok(()),
                    _ => Err(format!("{at}: expected non-empty string, got {actual}")),
                },
                "int" => {
                    if actual.is_i64() || actual.is_u64() {
                        Ok(())
                    } else {
                        Err(format!("{at}: expected integer, got {actual}"))
                    }
                }
                "number" => {
                    if actual.is_number() {
                        Ok(())
                    } else {
                        Err(format!("{at}: expected number, got {actual}"))
                    }
                }
                "bool" => {
                    if actual.is_boolean() {
                        Ok(())
                    } else {
                        Err(format!("{at}: expected bool, got {actual}"))
                    }
                }
                "sse" | "any" => Ok(()),
                other => panic!("{at}: unknown template @{other}"),
            };
        }
    }
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for key in e.keys() {
                if !a.contains_key(key) {
                    return Err(format!("{at}: missing key {key:?} in {actual}"));
                }
            }
            for key in a.keys() {
                if !e.contains_key(key) {
                    return Err(format!(
                        "{at}: unexpected key {key:?} (null-vs-absent drift)"
                    ));
                }
            }
            for (key, ev) in e {
                template_matches(ev, &a[key], &format!("{at}.{key}"))?;
            }
            Ok(())
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                return Err(format!(
                    "{at}: expected array of {}, got {} ({actual})",
                    e.len(),
                    a.len()
                ));
            }
            for (i, (ev, av)) in e.iter().zip(a.iter()).enumerate() {
                template_matches(ev, av, &format!("{at}[{i}]"))?;
            }
            Ok(())
        }
        (e, a) if e == a => Ok(()),
        (e, a) => Err(format!("{at}: expected {e}, got {a}")),
    }
}

/// `#bool`/`#array`/`#object`/`#nonempty` specials, else dotted navigation.
fn has_path(value: &Value, path: &str) -> bool {
    match path {
        "#bool" => value.is_boolean(),
        "#array" => value.is_array(),
        "#object" => value.is_object(),
        "#nonempty" => value.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        dotted => {
            let mut node = value;
            for segment in dotted.split('.') {
                node = match node {
                    Value::Object(map) => match map.get(segment) {
                        Some(v) => v,
                        None => return false,
                    },
                    Value::Array(items) => match segment.parse::<usize>() {
                        Ok(i) => match items.get(i) {
                            Some(v) => v,
                            None => return false,
                        },
                        Err(_) => return false,
                    },
                    _ => return false,
                };
            }
            true
        }
    }
}

fn request_headers(trace_step: &Value) -> Map<String, Value> {
    trace_step["request"]["headers"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

fn all_vars(trace: &Value) -> BTreeMap<String, String> {
    trace["vars"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Only the variables a step actually substituted into its args may template
/// the request (so a small captured id cannot corrupt an unrelated `limit`).
fn used_vars(trace: &Value) -> BTreeMap<String, String> {
    let all = all_vars(trace);
    let used: BTreeSet<&str> = trace["vars_used"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    all.into_iter()
        .filter(|(k, _)| used.contains(k.as_str()))
        .collect()
}

/// An absent or empty HTTP body is JSON `null` (error responses are bodyless).
fn response_json(body: Option<&str>) -> Value {
    match body {
        None | Some("") => Value::Null,
        Some(text) => serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("response body is not JSON: {e}: {text:?}")),
    }
}

/// Same, but a non-JSON body is treated as null (divergence-lock checks must
/// never panic on an extractor refusal frame).
fn response_json_lenient(body: Option<&str>) -> Value {
    match body {
        None | Some("") => Value::Null,
        Some(text) => serde_json::from_str(text).unwrap_or(Value::Null),
    }
}

fn compare_request(id: &str, scenario: &Value, trace: &Value, password: &str) {
    use base64::Engine as _;
    let vars = used_vars(trace);
    let request = &trace["request"];
    assert!(
        !request.is_null(),
        "{id}: the unmodified client sent no request (driver error: {})",
        trace["error"]
    );
    assert_eq!(
        request["method"], scenario["request"]["method"],
        "{id}: HTTP method drift"
    );
    let path = normalize_path(request["path"].as_str().unwrap(), &vars);
    assert_eq!(
        path, scenario["request"]["path"],
        "{id}: request path drift (expected {})",
        scenario["request"]["path"]
    );
    let query = normalize_query(request["query"].as_str().unwrap_or(""), &vars);
    assert_eq!(
        query,
        scenario["request"]["query"].as_str().unwrap_or(""),
        "{id}: query encoding drift"
    );
    let headers = request_headers(trace);
    if scenario["unauthenticated"] == true {
        assert!(
            !headers.contains_key("authorization"),
            "{id}: unauthenticated probe must carry no Authorization header"
        );
    } else {
        let expected_auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("kilo:{password}"))
        );
        assert_eq!(
            headers.get("authorization").and_then(|v| v.as_str()),
            Some(expected_auth.as_str()),
            "{id}: the frozen Basic auth header is byte-exact"
        );
    }
    assert!(
        !headers.contains_key("x-kilo-directory") && !headers.contains_key("x-kilo-workspace"),
        "{id}: no directory/workspace header may ride an unconfigured client"
    );
    let actual_ct = headers.get("content-type").and_then(|v| v.as_str());
    let golden_ct = scenario["request"]["content_type"].as_str();
    assert_eq!(actual_ct, golden_ct, "{id}: Content-Type drift");
    let actual_body = request["body"].as_str();
    let golden = &scenario["request"];
    let normalized_body = match actual_body {
        None => None,
        Some(text) => {
            let has_var = vars
                .values()
                .any(|v| !v.is_empty() && text.contains(v.as_str()));
            if has_var {
                Some(substitute_vars_in_body(text, &vars))
            } else {
                Some(text.to_string())
            }
        }
    };
    match golden.get("body") {
        None => assert!(
            actual_body.is_none(),
            "{id}: unexpected request body {actual_body:?}"
        ),
        Some(expected) => {
            let actual = normalized_body.as_deref().unwrap();
            if let Some(golden_text) = expected.as_str() {
                assert_eq!(
                    actual, golden_text,
                    "{id}: request body bytes drift (client serialization changed?)"
                );
            } else {
                let actual_json: Value = serde_json::from_str(actual)
                    .unwrap_or_else(|e| panic!("{id}: body not JSON: {e}"));
                template_matches(expected, &actual_json, id)
                    .unwrap_or_else(|e| panic!("{id}: request body: {e}"));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmodified_upstream_client_replays_against_the_real_daemon() {
    let freeze = std::env::var("FAKTOR_COMPAT_FREEZE_TRACES").as_deref() == Ok("1");
    // The report guard is created before anything can fail: a panic anywhere
    // in the replay still leaves failed evidence for the required CI lane.
    let mut report = new_report();
    let dir = tempfile::tempdir().unwrap();
    let (deps, password) = super::tests::server_deps(dir.path());
    let (handle, base) = super::tests::spawn_server(deps).await;

    if !node_available() {
        if std::env::var("FAKTOR_COMPAT_REQUIRE_NODE").as_deref() == Ok("1") {
            report.status = "failed";
            let _ = handle.shutdown.send(());
            panic!(
                "node is required for the unmodified-upstream-client replay \
                 (FAKTOR_COMPAT_REQUIRE_NODE=1) but was not found on PATH; \
                 target/certification/kilo-compat.json records status=failed"
            );
        }
        report.status = "skipped";
        eprintln!(
            "SKIP unmodified_upstream_client_replays_against_the_real_daemon: \
             node not found on PATH; the recorded sdk-traces goldens were not replayed \
             (set FAKTOR_COMPAT_REQUIRE_NODE=1 to make this fatal). \
             target/certification/kilo-compat.json records status=skipped"
        );
        let _ = handle.shutdown.send(());
        return;
    }

    let out = dir.path().join("sdk-client-traces.json");
    let node_base = base.clone();
    let node_password = password.as_str().to_string();
    let node_out = out.clone();
    let status = tokio::task::spawn_blocking(move || {
        Command::new("node")
            .arg("--import")
            .arg(js_dir().join("resolve-ts.mjs"))
            .arg(js_dir().join("run.mjs"))
            .env("KILO_BASE", &node_base)
            .env("KILO_PASSWORD", node_password)
            .env("KILO_TRACES", traces_root())
            .env("KILO_OUT", &node_out)
            .status()
    })
    .await
    .unwrap_or_else(|e| panic!("node task panicked: {e}"))
    .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    assert!(status.success(), "the unmodified client driver failed");
    let traces = load_json(&out);
    if let Ok(dump) = std::env::var("FAKTOR_COMPAT_TRACE_DUMP") {
        std::fs::write(dump, serde_json::to_string_pretty(&traces).unwrap()).unwrap();
    }
    let by_id: BTreeMap<String, Value> = traces["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap().to_string(), s.clone()))
        .collect();

    let mut checked = 0usize;
    for file in trace_files() {
        let mut group = load_json(&file);
        let mut steps = group["steps"].as_array().unwrap().clone();
        for step in steps.iter_mut() {
            let id = step["id"].as_str().unwrap().to_string();
            let trace = by_id
                .get(&id)
                .unwrap_or_else(|| panic!("{id}: the client driver produced no trace"));
            if freeze {
                step["request"] = freeze_request(trace, &used_vars(trace));
                step["response"] = freeze_response(trace, &all_vars(trace));
                checked += 1;
                continue;
            }
            compare_request(&id, step, trace, password.as_str());
            report.requests_passed += 1;
            let response = &trace["response"];
            assert!(
                !response.is_null(),
                "{id}: no HTTP response was recorded (driver error: {})",
                trace["error"]
            );
            let expected = &step["response"];
            assert_eq!(
                response["status"].as_u64().unwrap(),
                expected["status"].as_u64().unwrap(),
                "{id}: HTTP status drift (daemon {} vs golden {})",
                response["status"],
                expected["status"]
            );
            if expected["event_stream"] == true {
                assert!(
                    response["content_type"]
                        .as_str()
                        .unwrap_or("")
                        .contains("text/event-stream"),
                    "{id}: expected an SSE stream"
                );
            } else {
                let ct = response["content_type"].as_str().unwrap_or("");
                assert!(
                    ct.starts_with(expected["content_type"].as_str().unwrap_or("")),
                    "{id}: response Content-Type drift ({ct})"
                );
                if let Some(raw) = expected.get("body_text").and_then(|v| v.as_str()) {
                    assert_eq!(
                        response["body"].as_str().unwrap_or(""),
                        raw,
                        "{id}: raw response body drift"
                    );
                } else {
                    let actual: Value = response_json(response["body"].as_str());
                    template_matches(&expected["body"], &actual, &format!("{id}.response"))
                        .unwrap_or_else(|e| {
                            panic!("{id}: live response diverges from golden: {e}")
                        });
                }
            }
            if step["status"] == "divergence" {
                let sdk_requires = step["sdk_requires"].as_array().unwrap();
                let body: Value = if response["event_stream"] == true {
                    Value::Null
                } else {
                    response_json_lenient(response["body"].as_str())
                };
                let status_code = response["status"].as_u64().unwrap();
                let missing = sdk_requires
                    .iter()
                    .filter(|p| !has_path(&body, p.as_str().unwrap()))
                    .count();
                // SSE divergences cannot be field-locked from an idle stream
                // (no frames are emitted): the note carries that gap.
                let sse_note_locked = response["event_stream"] == true && sdk_requires.is_empty();
                let still_diverges = status_code != 200 || missing > 0 || sse_note_locked;
                assert!(
                    still_diverges,
                    "{id}: the daemon now appears SDK-compatible — promote this step to \
                     \"passing\" and update docs/wire-compat.md"
                );
            }
            assert_eq!(
                trace["request_count"].as_u64().unwrap(),
                1,
                "{id}: expected exactly one request from the client"
            );
            assert!(
                trace["error"].is_null() || response["status"].as_u64().unwrap() >= 400,
                "{id}: client-side error without an HTTP error response: {}",
                trace["error"]
            );
            if step["status"] == "passing" {
                report.responses_passed += 1;
            }
            checked += 1;
        }
        if freeze {
            group["steps"] = Value::Array(steps.clone());
            std::fs::write(&file, serde_json::to_string_pretty(&group).unwrap() + "\n")
                .unwrap_or_else(|e| panic!("cannot freeze {}: {e}", file.display()));
        }
    }
    assert!(checked >= 20, "trace corpus too thin: {checked} steps");
    report.status = if freeze {
        // Freeze runs regenerate goldens; they do not replay them.
        "skipped"
    } else if report.responses_passed == report.total {
        "passed"
    } else {
        "partial"
    };
    eprintln!(
        "kilo-compat report {}: requests {}/{} exact, responses {}/{} exact, \
         {} required divergences, status={}",
        report.path.display(),
        report.requests_passed,
        report.total,
        report.responses_passed,
        report.total,
        report.required_divergences,
        report.status,
    );
    let _ = handle.shutdown.send(());
}

/// Freeze one recorded request into the golden template form.
fn freeze_request(trace: &Value, vars: &BTreeMap<String, String>) -> Value {
    let request = &trace["request"];
    if request.is_null() {
        return Value::Null;
    }
    let headers = request_headers(trace);
    let mut golden = Map::new();
    golden.insert("method".into(), request["method"].clone());
    golden.insert(
        "path".into(),
        json!(normalize_path(request["path"].as_str().unwrap(), vars)),
    );
    golden.insert(
        "query".into(),
        json!(normalize_query(
            request["query"].as_str().unwrap_or(""),
            vars
        )),
    );
    if let Some(ct) = headers.get("content-type").and_then(|v| v.as_str()) {
        golden.insert("content_type".into(), json!(ct));
    }
    match request["body"].as_str() {
        None => {}
        Some(text) => {
            let has_var = vars
                .values()
                .any(|v| !v.is_empty() && text.contains(v.as_str()));
            if has_var {
                let parsed: Value = serde_json::from_str(text).unwrap();
                golden.insert("body".into(), normalize_json(&parsed, vars));
            } else {
                golden.insert("body".into(), json!(text));
            }
        }
    }
    Value::Object(golden)
}

/// Freeze one recorded response into the golden template form.
fn freeze_response(trace: &Value, vars: &BTreeMap<String, String>) -> Value {
    let response = &trace["response"];
    if response.is_null() {
        return Value::Null;
    }
    let mut golden = Map::new();
    golden.insert("status".into(), response["status"].clone());
    let event_stream = response["event_stream"] == true;
    let content_type = response["content_type"].as_str().unwrap_or("");
    let short_ct = content_type.split(';').next().unwrap_or("").trim();
    golden.insert("content_type".into(), json!(short_ct));
    golden.insert("event_stream".into(), json!(event_stream));
    if event_stream {
        golden.insert("body".into(), json!("@sse"));
    } else {
        let text = response["body"].as_str().unwrap_or("");
        if text.is_empty() {
            golden.insert("body".into(), Value::Null);
        } else if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            golden.insert("body".into(), normalize_json(&parsed, vars));
        } else {
            // Non-JSON refusal frames (axum query-extractor text) are kept
            // raw, never silently coerced.
            golden.insert("body_text".into(), json!(text));
        }
    }
    Value::Object(golden)
}
