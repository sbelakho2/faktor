use crate::util::{self, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

const MARKER_SCHEMA: &str = "faktor-woodpecker-lane/v2";

type HmacSha256 = Hmac<Sha256>;

#[derive(Serialize)]
struct Runner {
    os: String,
    arch: String,
    ci: String,
    run_id: String,
}

#[derive(Serialize, Clone)]
struct Artifact {
    path: String,
    sha256: String,
}

#[derive(Serialize, Clone)]
struct FlakeSummary {
    schema: String,
    status: String,
    count: usize,
    tests: Vec<String>,
    report: String,
}

#[derive(Serialize)]
struct Marker {
    schema: String,
    lane: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flakes: Option<FlakeSummary>,
    commit: String,
    tree: String,
    runner: Runner,
    started_at: String,
    finished_at: String,
    commands_b64: String,
    commands_digest: String,
    artifacts: Vec<Artifact>,
    artifact_digest: String,
    clean: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<String>,
}

fn artifact_lines(artifacts: &[Artifact]) -> String {
    let mut sorted = artifacts.to_vec();
    sorted.sort_by(|left, right| left.path.cmp(&right.path));
    let mut output = String::new();
    for artifact in sorted {
        output.push_str(&format!("{}\t{}\n", artifact.sha256, artifact.path));
    }
    output
}

fn artifact_digest(artifacts: &[Artifact]) -> String {
    format!(
        "sha256:{}",
        util::sha256_hex(artifact_lines(artifacts).as_bytes())
    )
}

fn compute_auth(value: &Value, token: &str) -> String {
    let payload = util::canonical_json(value);
    let mut mac = HmacSha256::new_from_slice(token.as_bytes()).expect("HMAC accepts any key");
    mac.update(payload.as_bytes());
    format!("hmac-sha256:{}", util::hex(&mac.finalize().into_bytes()))
}

fn lane_token_env_names(lane: &str) -> [String; 2] {
    let normalized = util::normalize_lane(lane);
    [
        format!("faktor_lane_token_{normalized}"),
        format!("FAKTOR_LANE_TOKEN_{}", normalized.to_uppercase()),
    ]
}

fn lane_token_from_env(lane: &str) -> String {
    for name in lane_token_env_names(lane) {
        if let Ok(value) = std::env::var(&name) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    String::new()
}

fn default_started_at() -> String {
    let raw = std::env::var("CI_PIPELINE_STARTED").unwrap_or_default();
    if let Some(value) = util::iso_from_epoch(&raw) {
        return value;
    }
    if !raw.is_empty() {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&raw) {
            return parsed
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
        }
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(&raw, "%Y-%m-%dT%H:%M:%S") {
            return parsed.and_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        }
    }
    util::now_iso()
}

fn default_runner_os() -> String {
    match std::env::consts::OS {
        "macos" => "darwin".into(),
        "windows" => "windows".into(),
        "linux" => "linux".into(),
        other => other.into(),
    }
}

fn default_runner_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64".into(),
        "amd64" => "x86_64".into(),
        "aarch64" => "arm64".into(),
        "arm64" => "arm64".into(),
        "x86" | "i386" | "i686" => "i686".into(),
        other => other.into(),
    }
}

fn git(args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|error| format!("git -C {}: {error}", cwd.display()))?;
    if !output.status.success() {
        return Err(format!(
            "git -C {} {}: {}",
            cwd.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn worktree_clean(cwd: &Path) -> bool {
    let commands: [&[&str]; 2] = [
        &["diff", "--exit-code", "--quiet"],
        &["diff", "--cached", "--exit-code", "--quiet"],
    ];
    for args in commands {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !status.is_ok_and(|status| status.success()) {
            return false;
        }
    }
    true
}

fn flag_value(args: &[String], name: &str, fallback: &str) -> String {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return fallback.to_string();
    };
    match args.get(index + 1) {
        Some(value) if !value.starts_with("--") => value.clone(),
        _ => String::new(),
    }
}

fn flag_values(args: &[String], name: &str) -> Vec<String> {
    let mut values = Vec::new();
    for (index, item) in args.iter().enumerate() {
        if item == name {
            if let Some(value) = args.get(index + 1) {
                if !value.starts_with("--") {
                    values.push(value.clone());
                }
            }
        }
    }
    values
}

fn read_stdin() -> String {
    if std::io::stdin().is_terminal() {
        return String::new();
    }
    let mut text = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut text);
    text
}

fn flake_summary(report: &Value, lane: &str, path: &str) -> Result<FlakeSummary> {
    let fail = |message: String| format!("--flake-file {path}: {message}");
    if !report.is_object() {
        return Err(fail("not a JSON object".into()));
    }
    let schema = report.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != "faktor-flake-tolerance/v1" {
        return Err(fail(format!(
            "schema '{schema}' is not faktor-flake-tolerance/v1"
        )));
    }
    let report_lane = report.get("lane").and_then(Value::as_str).unwrap_or("");
    if report_lane != lane {
        return Err(fail(format!("report lane '{report_lane}' is not '{lane}'")));
    }
    let status = report.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "clean" && status != "passed_with_flakes" {
        return Err(fail(format!(
            "status '{status}' cannot certify a passed lane"
        )));
    }
    let Some(flakes) = report.get("flakes").and_then(Value::as_array) else {
        return Err(fail("flakes is not an array".into()));
    };
    let mut tests = Vec::new();
    for flake in flakes {
        let Some(test) = flake.get("test").and_then(Value::as_str) else {
            return Err(fail("flake entry without a test name".into()));
        };
        if !test.is_empty()
            && flake.get("isolated_result").and_then(Value::as_str) == Some("passed")
            && flake.get("isolation_attempts").and_then(Value::as_u64) == Some(1)
        {
            tests.push(test.to_string());
            continue;
        }
        return Err(fail(format!(
            "flake '{test}' is not a once-retried isolated pass"
        )));
    }
    if status == "clean" && !tests.is_empty() {
        return Err(fail("clean report carries flakes".into()));
    }
    if status == "passed_with_flakes" && tests.is_empty() {
        return Err(fail("passed_with_flakes report carries no flakes".into()));
    }
    if report
        .get("persistent_failures")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
    {
        return Err(fail(
            "report carries persistent failures and cannot certify a passed lane".into(),
        ));
    }
    Ok(FlakeSummary {
        schema: schema.to_string(),
        status: status.to_string(),
        count: tests.len(),
        tests,
        report: path.to_string(),
    })
}

fn usage(to_stderr: bool) {
    let text = "usage: faktor-ci-tools lane-marker write --lane ID [--status passed|failed|skipped]\n  [--reason TEXT] [--artifact PATH]... [--optional-artifact PATH]...\n  [--flake-file target/certification/flakes/ID.json]\n  [--commands TEXT | --commands-file FILE] [--out FILE] [--cwd DIR]\n  [--commit SHA] [--tree SHA] [--run-id ID] [--started-at ISO] [--finished-at ISO]\n  [--runner-os OS] [--runner-arch ARCH] [--runner-ci CI]\n\nCommands default to stdin (pipe the lane's CMDS heredoc into write).\nToken: lane's own CI secret env (faktor_lane_token_<lane>, uppercase accepted).";
    if to_stderr {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
}

fn write_command(args: &[String]) -> Result<i32> {
    let cwd_text = flag_value(args, "--cwd", ".");
    let cwd = fs::canonicalize(&cwd_text).unwrap_or_else(|_| PathBuf::from(&cwd_text));
    let lane = flag_value(args, "--lane", "");
    if lane.is_empty() {
        usage(true);
        return Ok(2);
    }
    let commands_text = if !flag_value(args, "--commands-file", "").is_empty() {
        let file = flag_value(args, "--commands-file", "");
        util::read_text(&cwd.join(file))?
    } else if args.iter().any(|arg| arg == "--commands") {
        flag_value(args, "--commands", "")
    } else {
        read_stdin()
    };
    if commands_text.is_empty() {
        return Err(
            "no commands text: pipe the lane commands on stdin or pass --commands/--commands-file"
                .into(),
        );
    }
    let mut artifacts = Vec::new();
    for path in flag_values(args, "--artifact") {
        let full = cwd.join(&path);
        if !full.is_file() {
            return Err(format!(
                "--artifact {path} is not a file in {}",
                cwd.display()
            ));
        }
        artifacts.push(Artifact {
            path,
            sha256: util::sha256_file(&full)?,
        });
    }
    for path in flag_values(args, "--optional-artifact") {
        let full = cwd.join(&path);
        if full.is_file() {
            artifacts.push(Artifact {
                path,
                sha256: util::sha256_file(&full)?,
            });
        }
    }
    let flake_file = flag_value(args, "--flake-file", "");
    let flakes = if flake_file.is_empty() {
        None
    } else {
        let full = cwd.join(&flake_file);
        let parsed = util::read_json(&full)
            .map_err(|error| format!("--flake-file {flake_file}: {error}"))?;
        let summary = flake_summary(&parsed, &lane, &flake_file)?;
        if !artifacts.iter().any(|artifact| artifact.path == flake_file) {
            artifacts.push(Artifact {
                path: flake_file.clone(),
                sha256: util::sha256_file(&full)?,
            });
        }
        Some(summary)
    };
    let mut commit = flag_value(args, "--commit", "");
    if commit.is_empty() {
        commit = std::env::var("CI_COMMIT_SHA").unwrap_or_default();
    }
    if commit.is_empty() {
        commit = git(&["rev-parse", "HEAD"], &cwd)?;
    }
    let mut tree = flag_value(args, "--tree", "");
    if tree.is_empty() {
        tree = git(&["rev-parse", "HEAD^{tree}"], &cwd).unwrap_or_else(|_| "unknown".into());
    }
    let mut run_id = flag_value(args, "--run-id", "");
    if run_id.is_empty() {
        run_id = std::env::var("CI_PIPELINE_NUMBER").unwrap_or_else(|_| "0".into());
    }
    let status = flag_value(args, "--status", "passed");
    if status != "passed" && status != "failed" && status != "skipped" {
        return Err(format!("--status '{status}' is not passed|failed|skipped"));
    }
    let runner_ci = flag_value(args, "--runner-ci", "");
    let mut marker = Marker {
        schema: MARKER_SCHEMA.into(),
        lane: lane.clone(),
        status,
        reason: if args.iter().any(|arg| arg == "--reason") {
            Some(flag_value(args, "--reason", ""))
        } else {
            None
        },
        flakes,
        commit,
        tree,
        runner: Runner {
            os: {
                let value = flag_value(args, "--runner-os", "");
                if value.is_empty() {
                    default_runner_os()
                } else {
                    value
                }
            },
            arch: {
                let value = flag_value(args, "--runner-arch", "");
                if value.is_empty() {
                    default_runner_arch()
                } else {
                    value
                }
            },
            ci: if !runner_ci.is_empty() {
                runner_ci
            } else if std::env::var("CI").is_ok_and(|value| !value.is_empty()) {
                "woodpecker".into()
            } else {
                "local".into()
            },
            run_id,
        },
        started_at: {
            let value = flag_value(args, "--started-at", "");
            if value.is_empty() {
                default_started_at()
            } else {
                value
            }
        },
        finished_at: {
            let value = flag_value(args, "--finished-at", "");
            if value.is_empty() {
                util::now_iso()
            } else {
                value
            }
        },
        commands_b64: base64::engine::general_purpose::STANDARD.encode(commands_text.as_bytes()),
        commands_digest: format!("sha256:{}", util::sha256_hex(commands_text.as_bytes())),
        artifacts,
        artifact_digest: String::new(),
        clean: worktree_clean(&cwd),
        auth: None,
    };
    marker.artifact_digest = artifact_digest(&marker.artifacts);
    let token = lane_token_from_env(&lane);
    if !token.is_empty() {
        let unsigned = serde_json::to_value(&marker).map_err(|error| error.to_string())?;
        marker.auth = Some(compute_auth(&unsigned, &token));
    }
    let out_text = flag_value(
        args,
        "--out",
        &format!("target/certification/lanes/{lane}.json"),
    );
    let out = fs::canonicalize(cwd.join(&out_text)).unwrap_or_else(|_| cwd.join(&out_text));
    let text = serde_json::to_string(&marker).map_err(|error| error.to_string())?;
    util::fsync_write(&out, format!("{text}\n").as_bytes())?;
    println!(
        "lane-marker: {} lane={} status={} auth={}",
        out.display(),
        lane,
        marker.status,
        if marker.auth.is_some() {
            "hmac-sha256"
        } else {
            "none"
        }
    );
    Ok(0)
}

pub fn run(args: &[String]) -> i32 {
    if args.is_empty() {
        usage(true);
        return 2;
    }
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            usage(false);
            0
        }
        "write" => match write_command(&args[1..]) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("lane-marker: {error}");
                1
            }
        },
        other => {
            eprintln!("lane-marker: unknown command '{other}'");
            usage(true);
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::canonical_json;
    use serde_json::json;

    #[test]
    fn canonical_payload_sorts_keys_and_preserves_utf8() {
        let value = json!({"b": 1, "a": "é", "c": [true, null]});
        assert_eq!(canonical_json(&value), r#"{"a":"é","b":1,"c":[true,null]}"#);
    }

    #[test]
    fn artifact_digest_is_sorted_and_tab_separated() {
        let artifacts = vec![
            Artifact {
                path: "b".into(),
                sha256: "sha256:2".into(),
            },
            Artifact {
                path: "a".into(),
                sha256: "sha256:1".into(),
            },
        ];
        assert_eq!(artifact_lines(&artifacts), "sha256:1\ta\nsha256:2\tb\n");
    }
}
