use crate::util::{self, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPORT_SCHEMA: &str = "faktor-flake-tolerance/v1";
const ISOLATION_RETRIES_PER_TEST: u64 = 1;
const DEFAULT_MAX_FLAKES: u64 = 3;
const DEFAULT_SUITE_TIMEOUT_S: u64 = 4 * 3600;
const DEFAULT_ISOLATION_TIMEOUT_S: u64 = 900;
const EXCERPT_BYTES: usize = 2048;
const REFUSED_LANE_MARKERS: [&str; 2] = ["fault", "nightly"];
const REFUSED_ARG_MARKERS: [&str; 4] = ["--ignored", "[fault]", "[release]", "[visual]"];

fn bounded(text: &str, limit: usize) -> String {
    let text = text.trim_matches('\n');
    if text.len() <= limit {
        return text.to_string();
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n...[truncated]", &text[..cut])
}

#[derive(Default)]
struct ParseState {
    failed: Vec<String>,
    failed_set: HashSet<String>,
    excerpts: HashMap<String, String>,
    current_excerpt: Option<String>,
    excerpt_lines: Vec<String>,
    summary_failed: u64,
    summary_passed: u64,
}

impl ParseState {
    fn feed(&mut self, line: &str) {
        let raw = line.trim_end_matches('\n');
        if let Some(rest) = raw.strip_prefix("test result: ") {
            if let Some((ok_part, counters)) = rest.split_once(". ") {
                if ok_part == "ok" || ok_part == "FAILED" {
                    let mut passed = 0;
                    let mut failed = 0;
                    for part in counters.split(';') {
                        let part = part.trim();
                        if let Some(value) = part.strip_suffix(" passed") {
                            passed = value.trim().parse().unwrap_or(0);
                        } else if let Some(value) = part.strip_suffix(" failed") {
                            failed = value.trim().parse().unwrap_or(0);
                        }
                    }
                    self.summary_failed += failed;
                    self.summary_passed += passed;
                    self.close_excerpt();
                    return;
                }
            }
        }
        if let Some(name) = failed_name(raw) {
            if self.failed_set.insert(name.clone()) {
                self.failed.push(name.clone());
            }
            self.excerpts
                .entry(name)
                .or_insert_with(|| bounded(raw, EXCERPT_BYTES));
            return;
        }
        if let Some(name) = raw
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" stdout ----"))
        {
            self.close_excerpt();
            self.current_excerpt = Some(name.to_string());
            self.excerpt_lines.clear();
            return;
        }
        if raw.starts_with("---- ")
            || raw.starts_with("failures:")
            || raw.starts_with("test result: ")
        {
            self.close_excerpt();
            return;
        }
        if self.current_excerpt.is_some() {
            self.excerpt_lines.push(raw.to_string());
        }
    }

    fn close_excerpt(&mut self) {
        let Some(name) = self.current_excerpt.take() else {
            return;
        };
        let body = self.excerpt_lines.join("\n");
        let body = body.trim_matches('\n');
        if !body.is_empty() && self.failed_set.contains(&name) {
            self.excerpts.insert(name, bounded(body, EXCERPT_BYTES));
        }
        self.excerpt_lines.clear();
    }

    fn finish(&mut self) -> (Vec<String>, HashMap<String, String>, u64, u64) {
        self.close_excerpt();
        (
            self.failed.clone(),
            self.excerpts.clone(),
            self.summary_failed,
            self.summary_passed,
        )
    }
}

fn failed_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("test ")?;
    let index = rest.find(" ... FAILED")?;
    let name = &rest[..index];
    if name.is_empty() {
        return None;
    }
    let after = rest[index + " ... FAILED".len()..].chars().next();
    if after.is_some_and(|ch| ch.is_alphanumeric() || ch == '_') {
        return None;
    }
    Some(name.to_string())
}

fn ok_line(line: &str, name: &str) -> bool {
    let Some(rest) = line.strip_prefix("test ") else {
        return false;
    };
    let Some(index) = rest.find(" ... ok") else {
        return false;
    };
    let after = rest[index + " ... ok".len()..].chars().next();
    if after.is_some_and(|ch| ch.is_alphanumeric() || ch == '_') {
        return false;
    }
    rest[..index] == *name
}

fn process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

fn kill_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn spawn(argv: &[String]) -> Result<Child> {
    let Some(program) = argv.first() else {
        return Err("empty command".into());
    };
    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process_group(&mut command);
    command
        .spawn()
        .map_err(|error| format!("spawn {}: {error}", program))
}

fn start_watchdog(pid: u32, timeout_s: u64) -> (mpsc::Sender<()>, Arc<AtomicBool>) {
    let timed_out = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    if timeout_s > 0 {
        let flag = timed_out.clone();
        std::thread::spawn(move || {
            if rx.recv_timeout(Duration::from_secs(timeout_s)).is_err() {
                flag.store(true, Ordering::SeqCst);
                kill_tree(pid);
            }
        });
    }
    (tx, timed_out)
}

fn run_streaming(
    argv: &[String],
    log_path: &Path,
    timeout_s: u64,
) -> Result<(i32, ParseState, bool)> {
    let mut child = spawn(argv)?;
    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "child stdout unavailable".to_string())?;
    let stderr = child.stderr.take();
    let (tx, timed_out) = start_watchdog(pid, timeout_s);
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
    }
    let log =
        fs::File::create(log_path).map_err(|error| format!("{}: {error}", log_path.display()))?;
    let log = Arc::new(std::sync::Mutex::new(log));
    let stderr_log = log.clone();
    let stderr_thread = stderr.map(|stderr| {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let _ = std::io::stderr().write_all(line.as_bytes());
                        let _ = std::io::stderr().flush();
                        if let Ok(mut log) = stderr_log.lock() {
                            let _ = log.write_all(line.as_bytes());
                        }
                    }
                }
            }
        })
    });
    let mut state = ParseState::default();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let _ = std::io::stdout().write_all(line.as_bytes());
                let _ = std::io::stdout().flush();
                if let Ok(mut log) = log.lock() {
                    let _ = log.write_all(line.as_bytes());
                }
                state.feed(&line);
            }
        }
    }
    let status = child.wait().map_err(|error| format!("wait: {error}"))?;
    if let Some(handle) = stderr_thread {
        let _ = handle.join();
    }
    let _ = tx.send(());
    if timed_out.load(Ordering::SeqCst) {
        return Ok((124, state, true));
    }
    Ok((status.code().unwrap_or(1), state, false))
}

fn run_captured(argv: &[String], timeout_s: u64) -> Result<(i32, String, bool)> {
    let child = spawn(argv)?;
    let pid = child.id();
    let (tx, timed_out) = start_watchdog(pid, timeout_s);
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait: {error}"))?;
    let _ = tx.send(());
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if timed_out.load(Ordering::SeqCst) {
        return Ok((124, text, true));
    }
    Ok((output.status.code().unwrap_or(1), text, false))
}

fn observed_pass(name: &str, exit_code: i32, output: &str) -> bool {
    if exit_code != 0 {
        return false;
    }
    output.lines().any(|line| ok_line(line, name))
}

#[derive(Debug, Clone)]
struct RunOptions {
    lane: String,
    max_flakes: u64,
    report: String,
    suite_log: String,
    suite_timeout_seconds: u64,
    isolation_timeout_seconds: u64,
    command: Vec<String>,
}

fn validate_invocation(options: &RunOptions) -> Vec<String> {
    let mut problems = Vec::new();
    let normalized_lane = util::normalize_lane(&options.lane);
    for marker in REFUSED_LANE_MARKERS {
        if normalized_lane.contains(marker) {
            problems.push(format!(
                "lane '{}' is a {marker}-class proof lane; flake retries are never applied there",
                options.lane
            ));
        }
    }
    if options.command.is_empty() {
        problems.push("no suite command: pass it after --".into());
    } else {
        let argv0 = Path::new(&options.command[0])
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if argv0 != "cargo" || options.command.get(1).map(String::as_str) != Some("test") {
            problems.push(format!(
                "suite command must be `cargo test ...` (got '{}'); only the workspace test suite is re-runnable",
                options.command.join(" ")
            ));
        }
        if options.command.iter().any(|arg| arg == "--") {
            problems.push(
                "suite command already contains `--`; the isolation rerun appends libtest flags itself"
                    .into(),
            );
        }
        let joined = options.command.join(" ");
        for marker in REFUSED_ARG_MARKERS {
            if joined.contains(marker) {
                problems.push(format!(
                    "suite command carries '{marker}': fault/release/visual/ignored tests are never retried"
                ));
            }
        }
    }
    if options.max_flakes < 1 {
        problems.push("--max-flakes must be >= 1".into());
    }
    problems
}

fn isolation_argv(command: &[String], name: &str) -> Vec<String> {
    let mut argv = command.to_vec();
    argv.extend([
        "--".to_string(),
        "--exact".to_string(),
        name.to_string(),
        "--test-threads=1".to_string(),
    ]);
    argv
}

fn run(options: RunOptions) -> i32 {
    let problems = validate_invocation(&options);
    if !problems.is_empty() {
        eprintln!("flake-tolerance: REFUSED — {}", problems.join("; "));
        return 2;
    }
    let lane = options.lane.clone();
    let report_path = if options.report.is_empty() {
        format!("target/certification/flakes/{lane}.json")
    } else {
        options.report.clone()
    };
    let suite_log = if options.suite_log.is_empty() {
        format!("target/certification/flake-tolerance/{lane}-initial.log")
    } else {
        options.suite_log.clone()
    };
    let mut report = json!({
        "schema": REPORT_SCHEMA,
        "lane": lane,
        "status": "clean",
        "generated_at": util::now_iso(),
        "suite_command": options.command.join(" "),
        "suite_exit_code": 0,
        "initial_failure_count": 0,
        "retry_policy": {
            "isolation_retries_per_test": ISOLATION_RETRIES_PER_TEST,
            "max_flakes": options.max_flakes,
        },
        "flakes": [],
        "persistent_failures": [],
        "suite_log": suite_log,
    });
    println!(
        "flake-tolerance: lane={lane} suite: {}",
        options.command.join(" ")
    );
    let (exit_code, mut state, timed_out) = match run_streaming(
        &options.command,
        Path::new(&suite_log),
        options.suite_timeout_seconds,
    ) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("flake-tolerance: {error}");
            return 1;
        }
    };
    let (failed, excerpts, summary_failed, summary_passed) = state.finish();
    report["suite_exit_code"] = json!(exit_code);
    report["initial_failure_count"] = json!(failed.len());
    report["initial_summary"] = json!({
        "passed": summary_passed,
        "failed": summary_failed,
        "timed_out": timed_out,
    });
    if timed_out {
        report["status"] = json!("failed");
        report["reason"] = json!("suite-timeout");
        let _ = util::write_json(Path::new(&report_path), &report);
        eprintln!(
            "flake-tolerance: suite exceeded its {}s bound -> lane FAILS (no retries on a timed-out suite)",
            options.suite_timeout_seconds
        );
        return 1;
    }
    if exit_code == 0 {
        if !failed.is_empty() {
            report["status"] = json!("failed");
            report["reason"] = json!("failure-parse-mismatch");
            let _ = util::write_json(Path::new(&report_path), &report);
            eprintln!(
                "flake-tolerance: suite exit 0 but the output lists failed tests -> refusing to certify (parse inconsistency)"
            );
            return 1;
        }
        let _ = util::write_json(Path::new(&report_path), &report);
        println!("flake-tolerance: CLEAN — suite green, 0 flakes (report {report_path})");
        return 0;
    }
    if failed.is_empty() {
        report["status"] = json!("failed");
        report["reason"] = json!("no-test-failures-detected");
        let _ = util::write_json(Path::new(&report_path), &report);
        eprintln!(
            "flake-tolerance: suite failed without any identifiable test failure (build/tool error) -> lane FAILS; see {suite_log}"
        );
        return 1;
    }
    if summary_failed != failed.len() as u64 {
        report["status"] = json!("failed");
        report["reason"] = json!("failure-parse-mismatch");
        let _ = util::write_json(Path::new(&report_path), &report);
        eprintln!(
            "flake-tolerance: parsed {} failed test name(s) but the suite counters report {summary_failed} failed -> lane FAILS without retries (failures must be fully accounted for)",
            failed.len()
        );
        return 1;
    }
    if failed.len() as u64 > options.max_flakes {
        report["status"] = json!("failed");
        report["reason"] = json!("flake-budget-exceeded");
        let _ = util::write_json(Path::new(&report_path), &report);
        eprintln!(
            "flake-tolerance: {} failed tests exceed the flake budget {} -> lane FAILS without retries (a mass failure is a regression)",
            failed.len(),
            options.max_flakes
        );
        return 1;
    }
    println!(
        "flake-tolerance: initial run failed with {} test failure(s); re-running EACH name once, alone:",
        failed.len()
    );
    for name in &failed {
        let argv = isolation_argv(&options.command, name);
        let started = Instant::now();
        let (exit_code, output, iso_timed_out) =
            match run_captured(&argv, options.isolation_timeout_seconds) {
                Ok(result) => result,
                Err(error) => {
                    eprintln!("flake-tolerance: {error}");
                    return 1;
                }
            };
        let duration = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        eprintln!(
            "\nflake-tolerance: isolation `{}` -> exit {exit_code}{}",
            argv.join(" "),
            if iso_timed_out { " (TIMEOUT)" } else { "" }
        );
        eprint!("{output}");
        let result = if iso_timed_out {
            "timeout"
        } else if exit_code == 0 && !observed_pass(name, exit_code, &output) {
            "unverified-pass"
        } else if exit_code == 0 {
            "passed"
        } else {
            "failed"
        };
        if result == "passed" {
            report["flakes"].as_array_mut().unwrap().push(json!({
                "test": name,
                "initial_failure_excerpt": excerpts.get(name).cloned().unwrap_or_default(),
                "isolation_command": argv,
                "isolation_attempts": 1,
                "isolated_result": "passed",
                "isolation_duration_s": duration,
            }));
            println!(
                "flake-tolerance: [flake] {name} passed in isolation -> recorded (report {report_path})"
            );
        } else {
            report["persistent_failures"]
                .as_array_mut()
                .unwrap()
                .push(json!({
                    "test": name,
                    "initial_failure_excerpt": excerpts.get(name).cloned().unwrap_or_default(),
                    "isolation_command": argv,
                    "isolation_attempts": 1,
                    "isolated_result": result,
                    "isolation_excerpt": bounded(&output, EXCERPT_BYTES),
                }));
            eprintln!("flake-tolerance: [{result}] {name} — lane FAILS");
        }
    }
    if !report["persistent_failures"]
        .as_array()
        .is_some_and(Vec::is_empty)
    {
        report["status"] = json!("failed");
        report["reason"] = json!("persistent-in-isolation");
        let _ = util::write_json(Path::new(&report_path), &report);
        eprintln!(
            "flake-tolerance: FAIL — {} of {} failure(s) reproduced in isolation; real regressions are never tolerated",
            report["persistent_failures"].as_array().unwrap().len(),
            failed.len()
        );
        return 1;
    }
    report["status"] = json!("passed_with_flakes");
    let _ = util::write_json(Path::new(&report_path), &report);
    let names: Vec<String> = report["flakes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|flake| flake.get("test").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    println!(
        "flake-tolerance: PASS WITH FLAKES — {} flake(s) recorded, none reproduced in isolation: {}",
        names.len(),
        names.join(", ")
    );
    0
}

#[cfg(unix)]
const FAKE_CARGO: &str = r#"#!/usr/bin/env bash
set -u
dir="${FAKE_SCENARIO_DIR:?}"
isolation=0
for arg in "$@"; do
    if [ "$arg" = "--exact" ]; then isolation=1; fi
done
if [ "$isolation" = 1 ]; then
    printf '%s\n' "$*" >> "$dir/isolation.log"
    cat "$dir/isolation.out" 2>/dev/null || true
    exit "$(cat "$dir/isolation.rc" 2>/dev/null || printf 0)"
else
    printf '%s\n' "$*" >> "$dir/initial.log"
    cat "$dir/suite.out" 2>/dev/null || true
    exit "$(cat "$dir/suite.rc" 2>/dev/null || printf 0)"
fi
"#;

#[cfg(unix)]
fn selftest_scenario(
    tmp: &Path,
    name: &str,
    suite_out: &str,
    suite_rc: i32,
    isolation_out: Option<&str>,
    isolation_rc: Option<i32>,
) -> (PathBuf, PathBuf) {
    let scenario = tmp.join(name);
    fs::create_dir_all(&scenario).unwrap();
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let cargo = bin_dir.join("cargo");
    fs::write(&cargo, FAKE_CARGO).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(&cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&cargo, permissions).unwrap();
    fs::write(scenario.join("suite.out"), suite_out).unwrap();
    fs::write(scenario.join("suite.rc"), suite_rc.to_string()).unwrap();
    if let Some(out) = isolation_out {
        fs::write(scenario.join("isolation.out"), out).unwrap();
    }
    if let Some(rc) = isolation_rc {
        fs::write(scenario.join("isolation.rc"), rc.to_string()).unwrap();
    }
    (scenario, bin_dir)
}

#[cfg(unix)]
fn selftest_invoke(
    scenario: &Path,
    bin_dir: &Path,
    suite_extra: &[&str],
) -> (i32, Option<Value>, PathBuf, PathBuf) {
    let report = scenario.join("report.json");
    let log = scenario.join("suite.log");
    let mut path = bin_dir.display().to_string();
    path.push(':');
    path.push_str(&std::env::var("PATH").unwrap_or_default());
    let mut argv: Vec<String> = vec![
        "flake-tolerance".into(),
        "run".into(),
        "--lane".into(),
        "linux".into(),
        "--report".into(),
        report.display().to_string(),
        "--suite-log".into(),
        log.display().to_string(),
        "--isolation-timeout-seconds".into(),
        "30".into(),
        "--".into(),
        "cargo".into(),
        "test".into(),
        "--workspace".into(),
        "--all-features".into(),
    ];
    argv.extend(suite_extra.iter().map(|arg| arg.to_string()));
    let status = Command::new(std::env::current_exe().unwrap())
        .args(&argv)
        .env("PATH", path)
        .env("FAKE_SCENARIO_DIR", scenario)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    let payload = fs::read_to_string(&report)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    (
        status.code().unwrap_or(1),
        payload,
        report,
        scenario.to_path_buf(),
    )
}

#[cfg(unix)]
fn selftest() -> i32 {
    let mut checks: Vec<(bool, String)> = Vec::new();
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let flaky_name = "runtime::routing_tests::malicious_semantic_text_stays_data";
    let suite = format!(
        "test {flaky_name} ... FAILED\n\nfailures:\n\n---- {flaky_name} stdout ----\ntool write_file failed\nthread panicked at review must run\n\nfailures:\n    {flaky_name}\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 547 filtered out\n"
    );
    let isolation = format!(
        "test {flaky_name} ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 547 filtered out\n"
    );

    let (scenario, bin_dir) = selftest_scenario(
        tmp,
        "clean",
        "test a::one ... ok\ntest a::two ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        0,
        None,
        None,
    );
    let (code, payload, _, scenario) = selftest_invoke(&scenario, &bin_dir, &[]);
    checks.push((code == 0, "clean: exit 0".into()));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str)
            == Some("clean"),
        "clean: report status".into(),
    ));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("flakes"))
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty),
        "clean: no flakes recorded".into(),
    ));
    checks.push((
        !scenario.join("isolation.log").exists(),
        "clean: zero isolation invocations".into(),
    ));

    let (scenario, bin_dir) = selftest_scenario(tmp, "flaky", &suite, 1, Some(&isolation), Some(0));
    let (code, payload, report, scenario) = selftest_invoke(&scenario, &bin_dir, &[]);
    checks.push((code == 0, "flaky: exit 0 (lane green)".into()));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str)
            == Some("passed_with_flakes"),
        "flaky: report status passed_with_flakes".into(),
    ));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("flakes"))
            .and_then(Value::as_array)
            .is_some_and(|flakes| flakes.len() == 1),
        "flaky: exactly one recorded flake".into(),
    ));
    if let Some(flake) = payload
        .as_ref()
        .and_then(|p| p.get("flakes"))
        .and_then(Value::as_array)
        .and_then(|flakes| flakes.first())
    {
        checks.push((
            flake.get("test").and_then(Value::as_str) == Some(flaky_name),
            "flaky: name recorded".into(),
        ));
        checks.push((
            flake
                .get("initial_failure_excerpt")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("tool write_file failed")),
            "flaky: first failure excerpt recorded".into(),
        ));
        checks.push((
            flake.get("isolation_attempts").and_then(Value::as_u64) == Some(1),
            "flaky: exactly one isolation attempt".into(),
        ));
    }
    let isolation_lines: Vec<String> = fs::read_to_string(scenario.join("isolation.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    checks.push((
        isolation_lines.len() == 1,
        "flaky: exactly one isolation invocation".into(),
    ));
    checks.push((
        isolation_lines
            .first()
            .is_some_and(|line| line.contains(&format!("--exact {flaky_name}"))),
        "flaky: isolation rerun names the exact test".into(),
    ));
    checks.push((
        fs::read_to_string(&report)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| {
                value
                    .get("schema")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            == Some(REPORT_SCHEMA),
        "flaky: report schema".into(),
    ));

    let (scenario, bin_dir) =
        selftest_scenario(tmp, "persistent", &suite, 1, Some(&suite), Some(1));
    let (code, payload, _, _) = selftest_invoke(&scenario, &bin_dir, &[]);
    checks.push((code == 1, "persistent: exit 1 (lane red)".into()));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str)
            == Some("failed"),
        "persistent: report failed".into(),
    ));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("persistent_failures"))
            .and_then(Value::as_array)
            .is_some_and(|items| items.len() == 1),
        "persistent: recorded persistent failure".into(),
    ));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("flakes"))
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty),
        "persistent: no flake recorded".into(),
    ));

    let names = ["m::t0", "m::t1", "m::t2", "m::t3"];
    let mut mass = String::new();
    for name in names {
        mass.push_str(&format!("test {name} ... FAILED\n"));
    }
    mass.push_str(
        "test result: FAILED. 0 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out\n",
    );
    let (scenario, bin_dir) = selftest_scenario(tmp, "budget", &mass, 1, None, None);
    let (code, payload, _, scenario) = selftest_invoke(&scenario, &bin_dir, &[]);
    checks.push((code == 1, "budget: exit 1".into()));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("reason"))
            .and_then(Value::as_str)
            == Some("flake-budget-exceeded"),
        "budget: typed reason".into(),
    ));
    checks.push((
        !scenario.join("isolation.log").exists(),
        "budget: no retries past the flake budget".into(),
    ));

    let (scenario, bin_dir) = selftest_scenario(
        tmp,
        "build-error",
        "error[E0308]: mismatched types\nerror: could not compile `x`\n",
        101,
        None,
        None,
    );
    let (code, payload, _, scenario) = selftest_invoke(&scenario, &bin_dir, &[]);
    checks.push((code == 1, "build-error: exit 1".into()));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("reason"))
            .and_then(Value::as_str)
            == Some("no-test-failures-detected"),
        "build-error: typed reason".into(),
    ));
    checks.push((
        !scenario.join("isolation.log").exists(),
        "build-error: no retries".into(),
    ));

    let (scenario, bin_dir) = selftest_scenario(
        tmp,
        "unverified",
        &suite,
        1,
        Some("test result: ok. 0 passed; 0 failed; 0 ignored\n"),
        Some(0),
    );
    let (code, payload, _, _) = selftest_invoke(&scenario, &bin_dir, &[]);
    checks.push((code == 1, "unverified-pass: exit 1".into()));
    checks.push((
        payload
            .as_ref()
            .and_then(|p| p.get("persistent_failures"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("isolated_result"))
            .and_then(Value::as_str)
            == Some("unverified-pass"),
        "unverified-pass: recorded as unverified, never a flake".into(),
    ));

    let (scenario, bin_dir) = selftest_scenario(tmp, "ignored", &suite, 1, None, None);
    let (code, payload, report, scenario) = selftest_invoke(&scenario, &bin_dir, &["--ignored"]);
    checks.push((code == 2, "ignored: typed refusal (exit 2)".into()));
    checks.push((
        !scenario.join("initial.log").exists(),
        "ignored: suite never executed".into(),
    ));
    checks.push((
        !Path::new(&report).exists() && payload.is_none(),
        "ignored: no report written for a refusal".into(),
    ));

    for lane in ["fault-campaign", "fault-scale", "nightly"] {
        let (scenario, bin_dir) =
            selftest_scenario(tmp, &format!("lane-{lane}"), &suite, 1, None, None);
        let report = scenario.join("report.json");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "flake-tolerance",
                "run",
                "--lane",
                lane,
                "--report",
                report.to_str().unwrap(),
                "--suite-log",
                scenario.join("suite.log").to_str().unwrap(),
                "--",
                "cargo",
                "test",
                "--workspace",
                "--all-features",
            ])
            .env("FAKE_SCENARIO_DIR", &scenario)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        checks.push((
            status.code() == Some(2) && !scenario.join("initial.log").exists(),
            format!("refused lane '{lane}': exit 2 and suite never runs"),
        ));
    }

    let (scenario, bin_dir) = selftest_scenario(tmp, "noncargo", &suite, 1, None, None);
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "flake-tolerance",
            "run",
            "--lane",
            "linux",
            "--report",
            scenario.join("report.json").to_str().unwrap(),
            "--",
            "echo",
            "not a suite",
        ])
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    checks.push((
        status.code() == Some(2),
        "non-cargo command: typed refusal".into(),
    ));

    let failed: Vec<&String> = checks
        .iter()
        .filter(|(ok, _)| !ok)
        .map(|(_, message)| message)
        .collect();
    if !failed.is_empty() {
        println!(
            "\nflake-tolerance selftest: FAIL ({}/{} checks)",
            failed.len(),
            checks.len()
        );
        for message in failed {
            eprintln!("  FAIL: {message}");
        }
        return 1;
    }
    println!(
        "flake-tolerance selftest: PASS ({} checks; retry-once, flake recording, budget, unverified-pass and no-retry refusals proven)",
        checks.len()
    );
    0
}

#[cfg(not(unix))]
fn selftest() -> i32 {
    eprintln!("flake-tolerance selftest: skipped (requires POSIX process groups)");
    0
}

pub fn run_command(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("selftest") {
        return selftest();
    }
    if args.first().map(String::as_str) != Some("run") {
        eprintln!("usage: faktor-ci-tools flake-tolerance run --lane ID [options] -- cargo test ... | selftest");
        return 2;
    }
    let mut options = RunOptions {
        lane: String::new(),
        max_flakes: DEFAULT_MAX_FLAKES,
        report: String::new(),
        suite_log: String::new(),
        suite_timeout_seconds: DEFAULT_SUITE_TIMEOUT_S,
        isolation_timeout_seconds: DEFAULT_ISOLATION_TIMEOUT_S,
        command: Vec::new(),
    };
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            options.command = args[index + 1..].to_vec();
            break;
        }
        let next = args.get(index + 1);
        match arg.as_str() {
            "--lane" if next.is_some() => {
                options.lane = next.unwrap().clone();
                index += 2;
            }
            "--max-flakes" if next.is_some() => {
                options.max_flakes = next.unwrap().parse().unwrap_or(0);
                index += 2;
            }
            "--report" if next.is_some() => {
                options.report = next.unwrap().clone();
                index += 2;
            }
            "--suite-log" if next.is_some() => {
                options.suite_log = next.unwrap().clone();
                index += 2;
            }
            "--suite-timeout-seconds" if next.is_some() => {
                options.suite_timeout_seconds = next.unwrap().parse().unwrap_or(0);
                index += 2;
            }
            "--isolation-timeout-seconds" if next.is_some() => {
                options.isolation_timeout_seconds = next.unwrap().parse().unwrap_or(0);
                index += 2;
            }
            _ => index += 1,
        }
    }
    if options.lane.is_empty() {
        eprintln!("flake-tolerance: --lane is required");
        return 2;
    }
    run(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_failures_and_counters() {
        let mut state = ParseState::default();
        state.feed("test a::b ... FAILED\n");
        state.feed("---- a::b stdout ----\n");
        state.feed("boom\n");
        state.feed("test result: FAILED. 3 passed; 1 failed; 0 ignored\n");
        let (failed, excerpts, summary_failed, summary_passed) = state.finish();
        assert_eq!(failed, vec!["a::b"]);
        assert_eq!(excerpts["a::b"], "boom");
        assert_eq!(summary_failed, 1);
        assert_eq!(summary_passed, 3);
    }

    #[test]
    fn observed_pass_requires_the_exact_test_line() {
        assert!(observed_pass("a::b", 0, "test a::b ... ok\n"));
        assert!(!observed_pass("a::b", 0, "test a::bb ... ok\n"));
        assert!(!observed_pass("a::b", 1, "test a::b ... ok\n"));
    }

    #[test]
    fn refuses_fault_lanes_and_ignored_selection() {
        let options = RunOptions {
            lane: "fault-scale".into(),
            max_flakes: 3,
            report: String::new(),
            suite_log: String::new(),
            suite_timeout_seconds: 1,
            isolation_timeout_seconds: 1,
            command: vec!["cargo".into(), "test".into(), "--ignored".into()],
        };
        let problems = validate_invocation(&options);
        assert!(problems
            .iter()
            .any(|problem| problem.contains("fault-class")));
        assert!(problems.iter().any(|problem| problem.contains("--ignored")));
    }
}
