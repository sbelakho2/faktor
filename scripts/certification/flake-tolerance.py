#!/usr/bin/env python3
"""Bounded, recorded flake tolerance for the trusted linux test lane.

Why this exists: the trusted linux lane runs the ~600-test workspace suite on
a heavily emulated (Rosetta/VM) runner shared with every other lane. A single
load-sensitive test can stall past its own budget once per run (a different
test each time, each green in isolation), which pre-empts the dependent
vscode-e2e/certificate lanes. Blind retries would hide real regressions, so
this runner only ever re-runs the EXACT failed test names, once each, and only
after observing which tests actually failed.

Contract (documented in docs/certification.md):

  * the suite command after `--` is executed byte-for-byte as the lane
    declares it (the lane marker's command set is unchanged);
  * rc == 0                        -> status `clean`, exit 0;
  * rc != 0 with failed tests      -> each distinct failed test name is
    re-run EXACTLY once, alone (`--exact <name> --test-threads=1`), reusing
    the same suite command so cargo's cwd/env are byte-identical;
  * isolated run observed passing (`test <name> ... ok` in its output AND
    exit 0)                        -> recorded as a `flake`; lane stays green;
  * isolated run failing, timing out, or not observably running the test
                                   -> lane FAILS (exit 1): a real regression
    can never be masked, and an unverifiable pass is never a pass;
  * initial failures above the flake budget, or failures the parser cannot
    account for against the `test result:` counters -> lane FAILS with no
    retries (a mass failure is a regression, never flake weather);
  * refusals (typed, exit 2): non-`cargo test` suite commands, any
    `--ignored`/`[fault]`/`[soak]`/`[perf]` marker, and fault/soak/live/perf
    lanes — those lanes are never retried;
  * the report `faktor-flake-tolerance/v1` is ALWAYS written (even on a
    clean run) to target/certification/flakes/<lane>.json and is attached to
    the lane marker by lane-marker.sh --flake-file, so the flake list is part
    of the certification artifact and the marker HMAC covers it.

Usage:
  python3 scripts/certification/flake-tolerance.py run --lane linux \
      [--max-flakes 3] [--report target/certification/flakes/linux.json] \
      [--suite-log target/certification/flake-tolerance/linux-initial.log] \
      [--suite-timeout-seconds 14400] [--isolation-timeout-seconds 900] \
      -- cargo test --workspace --all-features
  python3 scripts/certification/flake-tolerance.py selftest
"""

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time

REPORT_SCHEMA = "faktor-flake-tolerance/v1"
# Exactly one isolation retry per failed test name; the TOTAL flake budget is
# the maximum number of distinct names the runner will retry at all.
ISOLATION_RETRIES_PER_TEST = 1
DEFAULT_MAX_FLAKES = 3
DEFAULT_SUITE_TIMEOUT_S = 4 * 3600
DEFAULT_ISOLATION_TIMEOUT_S = 900
EXCERPT_BYTES = 2048
# Lanes whose failures are never re-run: fault/soak/live/perf proof lanes.
REFUSED_LANE_MARKERS = ("fault", "soak", "live", "perf", "nightly")
REFUSED_ARG_MARKERS = ("--ignored", "[fault]", "[soak]", "[perf]", "[live]")

TEST_FAILED_RE = re.compile(r"^test (.+?) \.\.\. FAILED\b")
TEST_OK_RE = re.compile(r"^test (.+?) \.\.\. ok\b")
RESULT_RE = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored"
)
EXCERPT_START_RE = re.compile(r"^---- (.+?) stdout ----$")
EXCERPT_END_RE = re.compile(r"^(---- |failures:|test result: )")


def _bounded(text, limit=EXCERPT_BYTES):
    text = text.strip("\n")
    if len(text) <= limit:
        return text
    return text[:limit] + "\n...[truncated]"


class ParseState:
    """Streaming libtest output parser: failed names + first failure excerpts
    + the authoritative `test result:` counters. Bounded by construction: it
    retains at most one excerpt block per failed test."""

    def __init__(self):
        self.failed = []  # distinct names, first-seen order
        self._failed_set = set()
        self.excerpts = {}  # name -> excerpt text
        self._current_excerpt = None
        self._excerpt_lines = []
        self.summary_failed = 0
        self.summary_passed = 0

    def feed(self, line):
        raw = line.rstrip("\n")
        match = RESULT_RE.match(raw)
        if match:
            self.summary_failed += int(match.group(3))
            self.summary_passed += int(match.group(2))
            self._close_excerpt()
            return
        match = TEST_FAILED_RE.match(raw)
        if match:
            name = match.group(1)
            if name not in self._failed_set:
                self._failed_set.add(name)
                self.failed.append(name)
            if name not in self.excerpts:
                self.excerpts[name] = _bounded(raw)
            return
        match = EXCERPT_START_RE.match(raw)
        if match:
            self._close_excerpt()
            self._current_excerpt = match.group(1)
            self._excerpt_lines = []
            return
        if EXCERPT_END_RE.match(raw):
            self._close_excerpt()
            return
        if self._current_excerpt is not None:
            self._excerpt_lines.append(raw)

    def _close_excerpt(self):
        if self._current_excerpt is None:
            return
        name = self._current_excerpt
        body = "\n".join(self._excerpt_lines).strip("\n")
        if body and name in self._failed_set:
            self.excerpts[name] = _bounded(body)
        self._current_excerpt = None
        self._excerpt_lines = []

    def finish(self):
        self._close_excerpt()
        return self.failed, self.excerpts, self.summary_failed, self.summary_passed


def _popen(argv, env=None):
    return subprocess.Popen(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        env=env,
        start_new_session=True,
    )


def _kill_tree(proc):
    """Zero-orphan discipline: the suite/isolation child runs in its own
    process group, so a timeout kills the whole tree deliberately."""
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        proc.kill()


def _run_streaming(argv, log_path, timeout_s):
    """Run one command, stream stdout+stderr live, tee to log_path, and feed
    the parser. The wall-clock bound is a watchdog on the process group: it
    fires even when the child produces no output at all (a silent hang is
    bounded too). Returns (exit_code, ParseState, timed_out)."""
    state = ParseState()
    os.makedirs(os.path.dirname(log_path) or ".", exist_ok=True)
    with open(log_path, "w", encoding="utf-8", errors="replace") as log:
        proc = _popen(argv)
        timed_out = threading.Event()

        def _on_timeout():
            timed_out.set()
            _kill_tree(proc)

        timer = None
        if timeout_s:
            timer = threading.Timer(timeout_s, _on_timeout)
            timer.daemon = True
            timer.start()
        try:
            for line in proc.stdout:
                sys.stdout.write(line)
                log.write(line)
                state.feed(line)
        finally:
            if timer is not None:
                timer.cancel()
            try:
                rc = proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                _kill_tree(proc)
                rc = proc.wait()
    if timed_out.is_set():
        return 124, state, True
    return rc, state, False


def _run_captured(argv, timeout_s):
    """One isolation attempt: captured output, wall-clock bound, process-group
    kill on timeout. Returns (exit_code, output, timed_out)."""
    proc = _popen(argv)
    try:
        output, _ = proc.communicate(timeout=timeout_s)
        return proc.returncode, output or "", False
    except subprocess.TimeoutExpired:
        _kill_tree(proc)
        try:
            output, _ = proc.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            output = ""
        return 124, output or "", True


def _observed_pass(name, exit_code, output):
    """The isolation attempt counts as a PASS only when it exit-ed 0 AND the
    output actually shows that exact test passing. A zero-test run (a bad
    name/target filter) prints no such line and is NOT a pass."""
    if exit_code != 0:
        return False
    for line in output.splitlines():
        if TEST_OK_RE.match(line) and line.startswith("test {} ... ".format(name)):
            return True
    return False


def _refusal(problems):
    print("flake-tolerance: REFUSED — {}".format("; ".join(problems)), file=sys.stderr)
    return 2


def _validate_invocation(args):
    problems = []
    normalized_lane = re.sub(r"[^a-z0-9]+", "-", args.lane.lower())
    for marker in REFUSED_LANE_MARKERS:
        if marker in normalized_lane:
            problems.append(
                "lane '{}' is a {}-class proof lane; flake retries are never "
                "applied there".format(args.lane, marker)
            )
    if not args.command:
        problems.append("no suite command: pass it after --")
    else:
        argv0 = os.path.basename(args.command[0])
        if argv0 != "cargo" or len(args.command) < 2 or args.command[1] != "test":
            problems.append(
                "suite command must be `cargo test ...` (got '{}'); only the "
                "workspace test suite is re-runnable".format(" ".join(args.command))
            )
        if "--" in args.command:
            problems.append(
                "suite command already contains `--`; the isolation rerun "
                "appends libtest flags itself"
            )
        joined = " ".join(args.command)
        for marker in REFUSED_ARG_MARKERS:
            if marker in joined:
                problems.append(
                    "suite command carries '{}': fault/soak/perf/ignored tests "
                    "are never retried".format(marker)
                )
    if args.max_flakes < 1:
        problems.append("--max-flakes must be >= 1")
    return problems


def _write_report(path, report):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(report, indent=2, sort_keys=True) + "\n")


def _isolation_argv(suite_command, name):
    return list(suite_command) + ["--", "--exact", name, "--test-threads=1"]


def run(args):
    problems = _validate_invocation(args)
    if problems:
        return _refusal(problems)
    lane = args.lane
    report_path = args.report or "target/certification/flakes/{}.json".format(lane)
    suite_log = args.suite_log or "target/certification/flake-tolerance/{}-initial.log".format(
        lane
    )
    report = {
        "schema": REPORT_SCHEMA,
        "lane": lane,
        "status": "clean",
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "suite_command": " ".join(args.command),
        "suite_exit_code": 0,
        "initial_failure_count": 0,
        "retry_policy": {
            "isolation_retries_per_test": ISOLATION_RETRIES_PER_TEST,
            "max_flakes": args.max_flakes,
        },
        "flakes": [],
        "persistent_failures": [],
        "suite_log": suite_log,
    }
    print(
        "flake-tolerance: lane={} suite: {}".format(lane, " ".join(args.command)),
        flush=True,
    )
    exit_code, state, timed_out = _run_streaming(
        args.command, suite_log, args.suite_timeout_seconds
    )
    failed, excerpts, summary_failed, summary_passed = state.finish()
    report["suite_exit_code"] = exit_code
    report["initial_failure_count"] = len(failed)
    report["initial_summary"] = {
        "passed": summary_passed,
        "failed": summary_failed,
        "timed_out": timed_out,
    }

    if timed_out:
        report["status"] = "failed"
        report["reason"] = "suite-timeout"
        _write_report(report_path, report)
        print(
            "flake-tolerance: suite exceeded its {}s bound -> lane FAILS "
            "(no retries on a timed-out suite)".format(args.suite_timeout_seconds),
            file=sys.stderr,
        )
        return 1

    if exit_code == 0:
        if failed:
            report["status"] = "failed"
            report["reason"] = "failure-parse-mismatch"
            _write_report(report_path, report)
            print(
                "flake-tolerance: suite exit 0 but the output lists failed "
                "tests -> refusing to certify (parse inconsistency)",
                file=sys.stderr,
            )
            return 1
        _write_report(report_path, report)
        print(
            "flake-tolerance: CLEAN — suite green, 0 flakes "
            "(report {})".format(report_path)
        )
        return 0

    if not failed:
        report["status"] = "failed"
        report["reason"] = "no-test-failures-detected"
        _write_report(report_path, report)
        print(
            "flake-tolerance: suite failed without any identifiable test "
            "failure (build/tool error) -> lane FAILS; see {}".format(suite_log),
            file=sys.stderr,
        )
        return 1

    if summary_failed != len(failed):
        report["status"] = "failed"
        report["reason"] = "failure-parse-mismatch"
        _write_report(report_path, report)
        print(
            "flake-tolerance: parsed {} failed test name(s) but the suite "
            "counters report {} failed -> lane FAILS without retries "
            "(failures must be fully accounted for)".format(
                len(failed), summary_failed
            ),
            file=sys.stderr,
        )
        return 1

    if len(failed) > args.max_flakes:
        report["status"] = "failed"
        report["reason"] = "flake-budget-exceeded"
        _write_report(report_path, report)
        print(
            "flake-tolerance: {} failed tests exceed the flake budget {} -> "
            "lane FAILS without retries (a mass failure is a regression)".format(
                len(failed), args.max_flakes
            ),
            file=sys.stderr,
        )
        return 1

    print(
        "flake-tolerance: initial run failed with {} test failure(s); "
        "re-running EACH name once, alone:".format(len(failed)),
        flush=True,
    )
    for name in failed:
        isolation_argv = _isolation_argv(args.command, name)
        started = time.monotonic()
        exit_code, output, iso_timed_out = _run_captured(
            isolation_argv, args.isolation_timeout_seconds
        )
        duration = round(time.monotonic() - started, 3)
        print(
            "\nflake-tolerance: isolation `{}` -> exit {}{}".format(
                " ".join(isolation_argv), exit_code, " (TIMEOUT)" if iso_timed_out else ""
            ),
            file=sys.stderr,
        )
        sys.stderr.write(output)
        if iso_timed_out:
            result = "timeout"
        elif exit_code == 0 and not _observed_pass(name, exit_code, output):
            result = "unverified-pass"
        elif exit_code == 0:
            result = "passed"
        else:
            result = "failed"
        if result == "passed":
            report["flakes"].append(
                {
                    "test": name,
                    "initial_failure_excerpt": excerpts.get(name, ""),
                    "isolation_command": isolation_argv,
                    "isolation_attempts": 1,
                    "isolated_result": "passed",
                    "isolation_duration_s": duration,
                }
            )
            print(
                "flake-tolerance: [flake] {} passed in isolation -> recorded "
                "(report {})".format(name, report_path)
            )
        else:
            report["persistent_failures"].append(
                {
                    "test": name,
                    "initial_failure_excerpt": excerpts.get(name, ""),
                    "isolation_command": isolation_argv,
                    "isolation_attempts": 1,
                    "isolated_result": result,
                    "isolation_excerpt": _bounded(output),
                }
            )
            print(
                "flake-tolerance: [{}] {} — lane FAILS".format(result, name),
                file=sys.stderr,
            )

    if report["persistent_failures"]:
        report["status"] = "failed"
        report["reason"] = "persistent-in-isolation"
        _write_report(report_path, report)
        print(
            "flake-tolerance: FAIL — {} of {} failure(s) reproduced in "
            "isolation; real regressions are never tolerated".format(
                len(report["persistent_failures"]), len(failed)
            ),
            file=sys.stderr,
        )
        return 1

    report["status"] = "passed_with_flakes"
    _write_report(report_path, report)
    print(
        "flake-tolerance: PASS WITH FLAKES — {} flake(s) recorded, none "
        "reproduced in isolation: {}".format(
            len(report["flakes"]), ", ".join(item["test"] for item in report["flakes"])
        )
    )
    return 0


# --------------------------------------------------------------------------
# selftest: hermetic adversarial fixtures for the retry-once semantics.
# Every scenario drives the REAL run path through a fake `cargo` on PATH; the
# fake records each invocation so exactly-once retry, no-retry refusals and
# the unverified-pass refusal are all observable.
# --------------------------------------------------------------------------

_FAKE_CARGO = """#!/usr/bin/env bash
set -u
dir="${FAKE_SCENARIO_DIR:?}"
isolation=0
for arg in "$@"; do
    if [ "$arg" = "--exact" ]; then isolation=1; fi
done
if [ "$isolation" = 1 ]; then
    printf '%s\\n' "$*" >> "$dir/isolation.log"
    cat "$dir/isolation.out" 2>/dev/null || true
    exit "$(cat "$dir/isolation.rc" 2>/dev/null || printf 0)"
else
    printf '%s\\n' "$*" >> "$dir/initial.log"
    cat "$dir/suite.out" 2>/dev/null || true
    exit "$(cat "$dir/suite.rc" 2>/dev/null || printf 0)"
fi
"""


def _selftest_scenario(tmp, name, suite_out, suite_rc, isolation_out=None, isolation_rc=None):
    scenario = os.path.join(tmp, name)
    os.makedirs(scenario, exist_ok=True)
    bin_dir = os.path.join(tmp, "bin")
    os.makedirs(bin_dir, exist_ok=True)
    cargo = os.path.join(bin_dir, "cargo")
    with open(cargo, "w", encoding="utf-8") as handle:
        handle.write(_FAKE_CARGO)
    os.chmod(cargo, 0o755)
    with open(os.path.join(scenario, "suite.out"), "w", encoding="utf-8") as handle:
        handle.write(suite_out)
    with open(os.path.join(scenario, "suite.rc"), "w", encoding="utf-8") as handle:
        handle.write(str(suite_rc))
    if isolation_out is not None:
        with open(os.path.join(scenario, "isolation.out"), "w", encoding="utf-8") as handle:
            handle.write(isolation_out)
    if isolation_rc is not None:
        with open(os.path.join(scenario, "isolation.rc"), "w", encoding="utf-8") as handle:
            handle.write(str(isolation_rc))
    return scenario, bin_dir


def _selftest_invoke(tmp, scenario, bin_dir, suite_extra=()):
    report = os.path.join(scenario, "report.json")
    log = os.path.join(scenario, "suite.log")
    env = dict(os.environ)
    env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
    env["FAKE_SCENARIO_DIR"] = scenario
    argv = [
        sys.executable,
        os.path.abspath(__file__),
        "run",
        "--lane",
        "linux",
        "--report",
        report,
        "--suite-log",
        log,
        "--isolation-timeout-seconds",
        "30",
        "--",
        "cargo",
        "test",
        "--workspace",
        "--all-features",
        *suite_extra,
    ]
    proc = subprocess.run(argv, capture_output=True, text=True, env=env)
    payload = None
    if os.path.isfile(report):
        with open(report, encoding="utf-8") as handle:
            payload = json.load(handle)
    return proc, payload, report, scenario


def _selftest_assert(checks, condition, message):
    checks.append((bool(condition), message))


def selftest():
    checks = []
    with tempfile.TemporaryDirectory(prefix="flake-tolerance-selftest-") as tmp:
        # 1. clean suite -> exit 0, report clean, no isolation invocation.
        scenario, bin_dir = _selftest_scenario(
            tmp,
            "clean",
            "test a::one ... ok\ntest a::two ... ok\n"
            "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            0,
        )
        proc, payload, report, scenario = _selftest_invoke(tmp, scenario, bin_dir)
        _selftest_assert(checks, proc.returncode == 0, "clean: exit 0")
        _selftest_assert(checks, payload and payload["status"] == "clean", "clean: report status")
        _selftest_assert(
            checks, payload and payload["flakes"] == [], "clean: no flakes recorded"
        )
        _selftest_assert(
            checks,
            not os.path.exists(os.path.join(scenario, "isolation.log")),
            "clean: zero isolation invocations",
        )

        # 2. flaky-pass -> exit 0, one flake with name + excerpt, exactly ONE
        #    isolation invocation carrying the exact test name.
        flaky_name = "runtime::routing_tests::malicious_semantic_text_stays_data"
        suite = (
            "test {} ... FAILED\n"
            "\n"
            "failures:\n"
            "\n"
            "---- {} stdout ----\n"
            "tool write_file failed\n"
            "thread panicked at review must run\n"
            "\n"
            "failures:\n"
            "    {}\n"
            "\n"
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 547 filtered out\n"
        ).format(flaky_name, flaky_name, flaky_name)
        isolation = (
            "test {} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 547 filtered out\n"
        ).format(flaky_name)
        scenario, bin_dir = _selftest_scenario(
            tmp, "flaky", suite, 1, isolation_out=isolation, isolation_rc=0
        )
        proc, payload, report, scenario = _selftest_invoke(tmp, scenario, bin_dir)
        _selftest_assert(checks, proc.returncode == 0, "flaky: exit 0 (lane green)")
        _selftest_assert(
            checks,
            payload and payload["status"] == "passed_with_flakes",
            "flaky: report status passed_with_flakes",
        )
        _selftest_assert(
            checks,
            payload and len(payload["flakes"]) == 1,
            "flaky: exactly one recorded flake",
        )
        if payload and payload["flakes"]:
            flake = payload["flakes"][0]
            _selftest_assert(checks, flake["test"] == flaky_name, "flaky: name recorded")
            _selftest_assert(
                checks,
                "tool write_file failed" in flake["initial_failure_excerpt"],
                "flaky: first failure excerpt recorded",
            )
            _selftest_assert(
                checks,
                flake["isolation_attempts"] == 1,
                "flaky: exactly one isolation attempt",
            )
        with open(os.path.join(scenario, "isolation.log"), encoding="utf-8") as handle:
            isolation_lines = [line for line in handle.read().splitlines() if line]
        _selftest_assert(
            checks, len(isolation_lines) == 1, "flaky: exactly one isolation invocation"
        )
        _selftest_assert(
            checks,
            isolation_lines and "--exact {}".format(flaky_name) in isolation_lines[0],
            "flaky: isolation rerun names the exact test",
        )
        with open(report, encoding="utf-8") as handle:
            _selftest_assert(
                checks,
                json.load(handle)["schema"] == REPORT_SCHEMA,
                "flaky: report schema",
            )

        # 3. persistent failure -> exit 1, recorded persistent, no masking.
        scenario, bin_dir = _selftest_scenario(
            tmp, "persistent", suite, 1, isolation_out=suite, isolation_rc=1
        )
        proc, payload, report, scenario = _selftest_invoke(tmp, scenario, bin_dir)
        _selftest_assert(checks, proc.returncode == 1, "persistent: exit 1 (lane red)")
        _selftest_assert(
            checks, payload and payload["status"] == "failed", "persistent: report failed"
        )
        _selftest_assert(
            checks,
            payload and len(payload["persistent_failures"]) == 1,
            "persistent: recorded persistent failure",
        )
        _selftest_assert(
            checks, payload and payload["flakes"] == [], "persistent: no flake recorded"
        )

        # 4. budget: 4 failing tests > max-flakes 3 -> exit 1, ZERO retries.
        names = ["m::t{}".format(index) for index in range(4)]
        mass = "".join("test {} ... FAILED\n".format(name) for name in names)
        mass += "test result: FAILED. 0 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out\n"
        scenario, bin_dir = _selftest_scenario(tmp, "budget", mass, 1)
        proc, payload, report, scenario = _selftest_invoke(tmp, scenario, bin_dir)
        _selftest_assert(checks, proc.returncode == 1, "budget: exit 1")
        _selftest_assert(
            checks,
            payload and payload.get("reason") == "flake-budget-exceeded",
            "budget: typed reason",
        )
        _selftest_assert(
            checks,
            not os.path.exists(os.path.join(scenario, "isolation.log")),
            "budget: no retries past the flake budget",
        )

        # 5. build error (no identifiable test failure) -> exit 1, no retry.
        scenario, bin_dir = _selftest_scenario(
            tmp, "build-error", "error[E0308]: mismatched types\nerror: could not compile `x`\n", 101
        )
        proc, payload, report, scenario = _selftest_invoke(tmp, scenario, bin_dir)
        _selftest_assert(checks, proc.returncode == 1, "build-error: exit 1")
        _selftest_assert(
            checks,
            payload and payload.get("reason") == "no-test-failures-detected",
            "build-error: typed reason",
        )
        _selftest_assert(
            checks,
            not os.path.exists(os.path.join(scenario, "isolation.log")),
            "build-error: no retries",
        )

        # 6. unverified pass: isolation exits 0 but never shows the test
        #    running -> NOT a pass, lane fails (nothing is masked).
        scenario, bin_dir = _selftest_scenario(
            tmp,
            "unverified",
            suite,
            1,
            isolation_out="test result: ok. 0 passed; 0 failed; 0 ignored\n",
            isolation_rc=0,
        )
        proc, payload, report, scenario = _selftest_invoke(tmp, scenario, bin_dir)
        _selftest_assert(checks, proc.returncode == 1, "unverified-pass: exit 1")
        _selftest_assert(
            checks,
            payload
            and payload["persistent_failures"]
            and payload["persistent_failures"][0]["isolated_result"] == "unverified-pass",
            "unverified-pass: recorded as unverified, never a flake",
        )

        # 7. refusal: --ignored is a fault/ignored proof selection -> exit 2,
        #    the suite never runs and no report is written.
        scenario, bin_dir = _selftest_scenario(tmp, "ignored", suite, 1)
        proc, payload, report, scenario = _selftest_invoke(
            tmp, scenario, bin_dir, suite_extra=("--ignored",)
        )
        _selftest_assert(checks, proc.returncode == 2, "ignored: typed refusal (exit 2)")
        _selftest_assert(
            checks,
            not os.path.exists(os.path.join(scenario, "initial.log")),
            "ignored: suite never executed",
        )
        _selftest_assert(
            checks,
            not os.path.exists(os.path.join(scenario, "report.json")),
            "ignored: no report written for a refusal",
        )

        # 8. refusal: fault/soak/live/perf lanes are never retried.
        for lane in ("soak-smoke", "nightly", "perf", "fault-campaign", "live-paid"):
            scenario, bin_dir = _selftest_scenario(tmp, "lane-" + lane, suite, 1)
            env = dict(os.environ)
            env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
            env["FAKE_SCENARIO_DIR"] = scenario
            proc = subprocess.run(
                [
                    sys.executable,
                    os.path.abspath(__file__),
                    "run",
                    "--lane",
                    lane,
                    "--report",
                    os.path.join(scenario, "report.json"),
                    "--suite-log",
                    os.path.join(scenario, "suite.log"),
                    "--",
                    "cargo",
                    "test",
                    "--workspace",
                    "--all-features",
                ],
                capture_output=True,
                text=True,
                env=env,
            )
            _selftest_assert(
                checks,
                proc.returncode == 2
                and not os.path.exists(os.path.join(scenario, "initial.log")),
                "refused lane '{}': exit 2 and suite never runs".format(lane),
            )

        # 9. refusal: a non-cargo suite command can never be re-run by this
        #    runner.
        scenario, bin_dir = _selftest_scenario(tmp, "noncargo", suite, 1)
        env = dict(os.environ)
        env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
        proc = subprocess.run(
            [
                sys.executable,
                os.path.abspath(__file__),
                "run",
                "--lane",
                "linux",
                "--report",
                os.path.join(scenario, "report.json"),
                "--",
                "echo",
                "not a suite",
            ],
            capture_output=True,
            text=True,
            env=env,
        )
        _selftest_assert(checks, proc.returncode == 2, "non-cargo command: typed refusal")

    failed = [message for ok, message in checks if not ok]
    if failed:
        print("\nflake-tolerance selftest: FAIL ({}/{} checks)".format(len(failed), len(checks)))
        for message in failed:
            print("  FAIL: {}".format(message), file=sys.stderr)
        return 1
    print(
        "flake-tolerance selftest: PASS ({} checks; retry-once, flake recording, "
        "budget, unverified-pass and no-retry refusals proven)".format(len(checks))
    )
    return 0


def main(argv):
    parser = argparse.ArgumentParser(
        prog="flake-tolerance.py",
        description="Bounded, recorded flake tolerance (trusted linux test lane).",
    )
    sub = parser.add_subparsers(dest="command_name")
    run_parser = sub.add_parser("run")
    run_parser.add_argument("--lane", required=True)
    run_parser.add_argument("--max-flakes", type=int, default=DEFAULT_MAX_FLAKES)
    run_parser.add_argument("--report", default="")
    run_parser.add_argument("--suite-log", default="")
    run_parser.add_argument(
        "--suite-timeout-seconds", type=int, default=DEFAULT_SUITE_TIMEOUT_S
    )
    run_parser.add_argument(
        "--isolation-timeout-seconds", type=int, default=DEFAULT_ISOLATION_TIMEOUT_S
    )
    run_parser.add_argument("suite_command", nargs=argparse.REMAINDER)
    sub.add_parser("selftest")
    args = parser.parse_args(argv)
    if args.command_name == "run":
        command = list(args.suite_command)
        if command and command[0] == "--":
            command = command[1:]
        args.command = command
        return run(args)
    if args.command_name == "selftest":
        return selftest()
    parser.print_usage(sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
