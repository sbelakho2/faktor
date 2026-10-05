#!/usr/bin/env python3
# Faktor soak convergence sampler + checker (python3 stdlib only).
#
# Longevity acceptance is QUIESCENT CONVERGENCE, not merely "no crash". This
# tool is the single implementation behind that acceptance:
#
#   1. `sample` runs a BOUNDED sampler loop over a subject process tree and a
#      soak data directory, appending one JSONL sample per tick to `--out`:
#        - process tree (Linux /proc; `ps` fallback): aggregate RSS, open file
#          descriptors, live descendant count, process group id, orphan count
#          for the recorded pgid once the subject is gone;
#        - data directory: WAL bytes, temp file count, CAS blob count and the
#          CAS unreachable population (blob files not referenced by the store
#          database's artifact/checkpoint/attachment rows), read through a
#          read-only SQLite connection;
#        - optional HTTP reconnect probe against `--probe-url` using a FRESH
#          connection every tick (real close/reconnect, never a pooled one).
#      The loop is bounded by `--duration`, `--max-samples` and a signal
#      handler that takes one final quiescent sample + final scan on SIGTERM.
#      A missing python/sqlite/ps capability is a typed sample error, never a
#      silent skip.
#
#   2. `check` evaluates the sampler stream plus the workload's own metrics
#      stream (runtime gauges emitted by the process under churn: writer and
#      reader queue depth, background tasks, journal/index latency samples and
#      reconnect events) and emits typed pass/fail for every convergence
#      metric. ANY failed metric fails the lane:
#
#        rss_bounded
#        fds_bounded
#        child_processes_zero
#        background_tasks_settled
#        writer_queue_zero
#        reader_queue_zero
#        wal_converged
#        temp_files_removed
#        cas_unreachable_stable
#        journal_latency_no_upward_trend
#        index_latency_no_upward_trend
#        reconnect_correct
#        duration_target_met              (when --target-seconds is supplied)
#
#      Missing samples for a metric are a FAILED metric
#      (`missing-metric-samples`), never a skip: the samplers above exist and
#      production lanes must produce the data.
#
#   3. `selftest` proves the metric algebra with fake metrics (pass and fail
#      worlds); `scripts/soak.sh --selftest` additionally exercises the whole
#      driver integration against fake cargo output.
#
# Usage:
#   python3 scripts/certification/soak-convergence.py sample \
#     --out F.jsonl [--pid PID | --pidfile F] [--data-dir DIR] \
#     [--probe-url URL] [--interval 1] [--duration 60] [--max-samples 100000] \
#     [--final-scan F.json]
#   python3 scripts/certification/soak-convergence.py check \
#     --samples F.jsonl [--workload F.jsonl] [--final F.json] \
#     [--lane L] [--mode M] [--commit SHA] [--tree TREE] \
#     [--elapsed-seconds N] [--target-seconds N] [--out F.json]
#   python3 scripts/certification/soak-convergence.py selftest
#
# Exit codes: 0 pass; 1 failed metric / refused sample; 2 usage error.

import argparse
import json
import os
import re
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.request

SCHEMA = "faktor-soak-convergence/v1"
SAMPLER_SCHEMA = "faktor-soak-samples/v1"
MAX_WALK_ENTRIES = 200_000
MAX_REFERENCE_ROWS = 500_000
MAX_STREAM_BYTES = 64 * 1024 * 1024
MIN_SAMPLES = 4

# Documented convergence bands. These are the numbers recorded in
# target/certification/soak.json and the ones docs/certification.md cites.
BANDS = {
    "rss_growth_factor": 1.5,
    "rss_growth_slack_kb": 128 * 1024,
    "rss_absolute_cap_kb": 2 * 1024 * 1024,
    "fd_slack_min": 16,
    "fd_fraction": 0.10,
    "fd_absolute_cap": 4096,
    "wal_window_slack_bytes": 4 * 1024 * 1024,
    "wal_absolute_bound_bytes": 64 * 1024 * 1024,
    "cas_unreachable_slack": 8,
    "latency_factor": 2.0,
    "latency_abs_cap_us": 500_000,
    "queue_population": 0,
    "background_tasks_population": 0,
    "child_processes": 0,
    "temp_files": 0,
    "reconnect_failures": 0,
}


def fail(message):
    print(f"soak-convergence: REFUSED: {message}", file=sys.stderr)
    return 1


def iso_now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


# ----------------------------------------------------------------- sampling


def _read_int(path):
    try:
        with open(path, "r", encoding="ascii", errors="replace") as fh:
            return fh.read().strip()
    except OSError:
        return None


def process_table_linux():
    """pid -> (ppid, rss_kb, pgid, state) from /proc, bounded."""
    table = {}
    try:
        names = os.listdir("/proc")
    except OSError:
        return None
    for name in names[:MAX_WALK_ENTRIES]:
        if not name.isdigit():
            continue
        pid = int(name)
        try:
            with open(f"/proc/{pid}/stat", "rb") as fh:
                raw = fh.read()
        except OSError:
            continue
        # comm may contain spaces/parens: split on the LAST ')'.
        close = raw.rfind(b")")
        if close < 0:
            continue
        fields = raw[close + 1:].split()
        if len(fields) < 20:
            continue
        try:
            table[pid] = {
                "ppid": int(fields[1]),
                "pgid": int(fields[2]),
                # /proc/<pid>/stat field 24 (rss pages); after the closing
                # ')' the state field is index 0, so rss is index 21.
                "rss_kb": int(fields[21]) * (os.sysconf("SC_PAGE_SIZE") // 1024),
                "state": fields[0].decode("ascii", "replace"),
            }
        except (ValueError, OSError):
            continue
    return table


def process_table_ps():
    """pid -> (ppid, rss_kb) via ps (macOS/BSD fallback), bounded."""
    try:
        out = subprocess.run(
            ["ps", "-eo", "pid=,ppid=,rss="],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    table = {}
    for line in out.stdout.splitlines()[:MAX_WALK_ENTRIES]:
        parts = line.split()
        if len(parts) < 3:
            continue
        try:
            pid, ppid, rss = int(parts[0]), int(parts[1]), int(parts[2])
        except ValueError:
            continue
        table[pid] = {"ppid": ppid, "pgid": pid, "rss_kb": rss, "state": "?"}
    return table or None


def fd_count(pid):
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        pass
    try:
        out = subprocess.run(
            ["lsof", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if out.returncode == 0:
            return max(0, len(out.stdout.splitlines()) - 1)
    except (OSError, subprocess.SubprocessError):
        pass
    return None


def process_metrics(root_pid):
    """Aggregate RSS/fds/descendants for the root pid's tree."""
    table = process_table_linux()
    if table is None:
        table = process_table_ps()
    if table is None:
        return {"sample_error": "no-process-sampler"}
    if root_pid not in table:
        return {"root_alive": False}
    members = {root_pid}
    frontier = [root_pid]
    while frontier:
        pid = frontier.pop()
        for candidate, entry in table.items():
            if entry["ppid"] == pid and candidate not in members:
                members.add(candidate)
                frontier.append(candidate)
                if len(members) > MAX_WALK_ENTRIES:
                    break
    rss_kb = 0
    fds = 0
    for pid in members:
        rss_kb += table[pid].get("rss_kb", 0)
        count = fd_count(pid)
        if count is not None:
            fds += count
    pgid = table[root_pid].get("pgid")
    return {
        "root_alive": True,
        "pgid": pgid,
        "rss_kb": rss_kb,
        "fds": fds,
        "children": len(members) - 1,
    }


def pgid_members(pgid):
    if pgid is None:
        return None
    table = process_table_linux() or process_table_ps() or {}
    return sum(1 for entry in table.values() if entry.get("pgid") == pgid)


TEMP_PATTERNS = (
    re.compile(r"\.tmp$"),
    re.compile(r"\.temp$"),
    re.compile(r"\.part$"),
    re.compile(r"\.partial$"),
    re.compile(r"\.incomplete$"),
    re.compile(r"\.download$"),
    re.compile(r"-journal$"),
    re.compile(r"^tmp[_-]"),
)
SHARD = re.compile(r"^[0-9a-f]{2}$")
BLOB = re.compile(r"^[0-9a-f]{64}$")


def _is_temp(name):
    return any(pattern.search(name) for pattern in TEMP_PATTERNS)


def _walk_bounded(root):
    """Yield (path, size, name, parent_name) under root, bounded."""
    count = 0
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames.sort()
        filenames.sort()
        parent = os.path.basename(dirpath)
        for name in filenames:
            count += 1
            if count > MAX_WALK_ENTRIES:
                return
            full = os.path.join(dirpath, name)
            try:
                size = os.lstat(full).st_size
            except OSError:
                continue
            yield full, size, name, parent


def find_store_db(data_dir):
    direct = os.path.join(data_dir, "store", "faktor-plus.db")
    if os.path.isfile(direct):
        return direct
    for path, _size, name, _parent in _walk_bounded(data_dir):
        if name == "faktor-plus.db":
            return path
    return None


def find_cas_root(data_dir):
    direct = os.path.join(data_dir, "cas")
    if os.path.isdir(direct):
        return direct
    return None


def store_references(db_path):
    """Every CAS hash the store schema references, bounded."""
    if not db_path:
        return None
    refs = set()
    uri = f"file:{db_path}?mode=ro"
    try:
        conn = sqlite3.connect(uri, uri=True, timeout=5)
    except sqlite3.Error:
        return None
    try:
        conn.execute("PRAGMA query_only=1")
        for sql in (
            ("SELECT cas_hash FROM artifact LIMIT ?", "cas_hash"),
            (
                "SELECT after_cas_hash FROM checkpoint WHERE after_cas_hash IS NOT NULL LIMIT ?",
                "after_cas_hash",
            ),
            ("SELECT digest FROM attachment LIMIT ?", "digest"),
        ):
            try:
                cursor = conn.execute(sql[0], (MAX_REFERENCE_ROWS,))
            except sqlite3.Error:
                continue
            for (value,) in cursor:
                if not isinstance(value, str):
                    continue
                normalized = value.strip().lower()
                if ":" in normalized:
                    normalized = normalized.split(":")[-1]
                if BLOB.match(normalized):
                    refs.add(normalized)
                if len(refs) >= MAX_REFERENCE_ROWS:
                    break
    finally:
        conn.close()
    return refs


def data_dir_metrics(data_dir, refs_cache):
    metrics = {
        "wal_bytes": 0,
        "temp_files": 0,
        "cas_blobs": 0,
        "cas_unreachable": None,
    }
    if not data_dir or not os.path.isdir(data_dir):
        metrics["sample_error"] = "no-data-dir"
        return metrics
    store_db = find_store_db(data_dir)
    root = os.path.dirname(store_db) if store_db else data_dir
    if os.path.isdir(root):
        for _path, size, name, _parent in _walk_bounded(root):
            if name.endswith("-wal"):
                metrics["wal_bytes"] += size
            elif _is_temp(name):
                metrics["temp_files"] += 1
    cas_root = find_cas_root(data_dir)
    if cas_root:
        blobs = []
        for _path, _size, name, parent in _walk_bounded(cas_root):
            if BLOB.match(name) and SHARD.match(parent):
                blobs.append(name)
        metrics["cas_blobs"] = len(blobs)
        if "refs" not in refs_cache:
            refs_cache["refs"] = store_references(store_db)
        refs = refs_cache["refs"]
        if refs is None:
            metrics["cas_unreachable"] = None
            metrics["sample_error"] = "store-reference-scan-unavailable"
        else:
            metrics["cas_unreachable"] = sum(1 for blob in blobs if blob not in refs)
    return metrics


def reconnect_probe(url):
    if not url:
        return None
    try:
        request = urllib.request.Request(url, headers={"Connection": "close"})
        with urllib.request.urlopen(request, timeout=5) as response:
            response.read(4096)
        return True
    except Exception:  # noqa: BLE001 - any probe failure is a typed false
        return False


class Sampler:
    def __init__(self, args):
        self.args = args
        self.stop = False
        self.samples = 0
        self.refs_cache = {}
        self.last_pgid = None
        self.final_scan = None

    def _pid(self):
        if self.args.pid:
            return (self.args.pid, "argument")
        # The workload's own subject pidfile wins: a convergence-aware
        # workload publishes its real pid there, so the wrapper/compile
        # process tree behind the root pidfile never pollutes process
        # metrics.
        if self.args.pidfile and os.path.isfile(self.args.pidfile):
            raw = _read_int(self.args.pidfile)
            if raw and raw.isdigit():
                return (int(raw), "subject")
        if self.args.root_pidfile and os.path.isfile(self.args.root_pidfile):
            raw = _read_int(self.args.root_pidfile)
            if raw and raw.isdigit():
                return (int(raw), "root")
        return None

    def tick(self, final=False):
        sample = {
            "schema": SAMPLER_SCHEMA,
            "t": int(time.time() * 1000),
            "elapsed_ms": int((time.time() - self.started) * 1000),
        }
        resolved = self._pid()
        root = resolved[0] if resolved else None
        if root is not None:
            proc = process_metrics(root)
            sample.update(proc)
            if proc.get("root_alive"):
                sample["pid"] = root
                sample["pid_source"] = resolved[1]
            if proc.get("pgid") is not None:
                self.last_pgid = proc["pgid"]
        else:
            sample["root_alive"] = False
            # A gone root has no live descendants; any lingering group member
            # is counted through the orphan scan below instead.
            sample["children"] = 0
            if self.last_pgid is not None:
                sample["pgid"] = self.last_pgid
        if self.last_pgid is not None and not sample.get("root_alive", False):
            sample["orphans"] = pgid_members(self.last_pgid) or 0
        else:
            sample["orphans"] = 0 if sample.get("root_alive") else None
        sample.update(data_dir_metrics(self.args.data_dir, self.refs_cache))
        probe = reconnect_probe(self.args.probe_url)
        if probe is not None:
            sample["probe_ok"] = probe
        if final:
            sample["final"] = True
            self.final_scan = {
                "schema": "faktor-soak-final-scan/v1",
                "t": sample["t"],
                "orphans": sample.get("orphans"),
                "pgid": self.last_pgid,
                "wal_bytes": sample.get("wal_bytes"),
                "temp_files": sample.get("temp_files"),
                "cas_blobs": sample.get("cas_blobs"),
                "cas_unreachable": sample.get("cas_unreachable"),
            }
            if self.args.final_scan:
                _write_json_atomic(self.args.final_scan, self.final_scan)
        _append_jsonl(self.args.out, sample)
        self.samples += 1

    def run(self):
        self.started = time.time()
        interval = min(max(self.args.interval, 0.2), 60.0)
        duration = min(max(self.args.duration, 0.2), 7 * 24 * 3600)
        deadline = self.started + duration

        def _on_signal(_signum, _frame):
            self.stop = True

        signal.signal(signal.SIGTERM, _on_signal)
        signal.signal(signal.SIGINT, _on_signal)
        try:
            while not self.stop:
                self.tick()
                if self.samples >= self.args.max_samples:
                    break
                now = time.time()
                if now >= deadline:
                    break
                # Wait until the next tick, waking early on a stop request.
                while not self.stop and time.time() < min(now + interval, deadline):
                    time.sleep(0.1)
        finally:
            self.tick(final=True)
        return 0


def _append_jsonl(path, payload):
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True) + "\n")


def _write_json_atomic(path, payload):
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    tmp = f"{path}.tmp.{os.getpid()}"
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    os.replace(tmp, path)


# ---------------------------------------------------------------- checking


def read_jsonl_bounded(path, max_bytes=MAX_STREAM_BYTES):
    """Read a JSONL stream; when oversized keep the TAIL (quiescence wins)."""
    if not path or not os.path.isfile(path):
        return [], False
    size = os.path.getsize(path)
    truncated = size > max_bytes
    with open(path, "rb") as fh:
        if truncated:
            fh.seek(size - max_bytes)
            fh.readline()  # drop the partial first line
        raw = fh.read(max_bytes)
    records = []
    for line in raw.decode("utf-8", "replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict):
            records.append(record)
    return records, truncated


def read_json_file(path):
    if not path or not os.path.isfile(path):
        return None
    try:
        with open(path, "r", encoding="utf-8") as fh:
            value = json.load(fh)
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def percentile(values, pct):
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return float(ordered[0])
    rank = (len(ordered) - 1) * (pct / 100.0)
    low = int(rank)
    high = min(low + 1, len(ordered) - 1)
    frac = rank - low
    return float(ordered[low] + (ordered[high] - ordered[low]) * frac)


def median(values):
    if not values:
        return None
    ordered = sorted(values)
    count = len(ordered)
    middle = count // 2
    if count % 2:
        return float(ordered[middle])
    return (ordered[middle - 1] + ordered[middle]) / 2.0


def windows(records, key):
    """(first, last, quintuples) values of `key` over present samples."""
    present = [record[key] for record in records if isinstance(record.get(key), (int, float))]
    if not present:
        return None
    # At least two samples per window: a one-sample window makes percentile
    # comparison meaningless noise.
    cut = max(2, len(present) // 4)
    first = present[:cut]
    last = present[-cut:]
    quint = max(1, len(present) // 5)
    groups = [present[i:i + quint] for i in range(0, len(present), quint)][:5]
    return first, last, groups


def dominant_subject(samples):
    """The workload pid: samples the workload itself published through its
    subject pidfile win outright; only when no such sample exists (a
    non-convergence-aware workload) fall back to the most-sampled pid, so
    build/tooling wrappers never pollute the process windows."""
    pool = [sample for sample in samples if sample.get("pid_source") == "subject"]
    if not pool:
        pool = samples
    counts = {}
    for sample in pool:
        pid = sample.get("pid")
        if isinstance(pid, int):
            counts[pid] = counts.get(pid, 0) + 1
    if not counts:
        return None
    return max(counts.items(), key=lambda item: (item[1], item[0]))[0]


def metric(name, passed, observed, expected, evidence, reason=""):
    entry = {
        "name": name,
        "status": "passed" if passed else "failed",
        "observed": observed,
        "expected": expected,
        "evidence": evidence,
    }
    if reason:
        entry["reason"] = reason
    return entry


def check_metric_missing(name, evidence, expected):
    return metric(name, False, {}, expected, evidence, reason="missing-metric-samples")


def build_report(args):
    samples, samples_truncated = read_jsonl_bounded(args.samples)
    workload, workload_truncated = read_jsonl_bounded(args.workload)
    final = read_json_file(args.final) or {}
    metrics = []
    evidence_common = {
        "samples_file": os.path.abspath(args.samples) if args.samples else "",
        "samples": len(samples),
        "samples_truncated": samples_truncated,
        "workload_file": os.path.abspath(args.workload) if args.workload else "",
        "workload_samples": len(workload),
        "workload_truncated": workload_truncated,
    }

    # ---------------------------------------------------------- process RSS
    # The baseline is the steady-state median of the per-quintile maxima
    # (startup/build transients and the quiescent tail are separate windows),
    # so build-tool RSS never masquerades as churn growth. Process windows are
    # restricted to the dominant subject pid, so compile/tooling processes
    # that appear behind the pidfile are excluded.
    subject_pid = dominant_subject(samples)
    subject = [s for s in samples if s.get("pid") == subject_pid] if subject_pid else samples
    evidence_common["subject_pid"] = subject_pid
    rss = windows(subject, "rss_kb")
    if not rss or len([s for s in subject if isinstance(s.get("rss_kb"), (int, float))]) < MIN_SAMPLES:
        metrics.append(check_metric_missing("rss_bounded", evidence_common, {
            "growth_factor": BANDS["rss_growth_factor"],
            "growth_slack_kb": BANDS["rss_growth_slack_kb"],
            "absolute_cap_kb": BANDS["rss_absolute_cap_kb"],
            "min_samples": MIN_SAMPLES,
        }))
    else:
        _first, last, groups = rss
        maxima = [max(group) for group in groups if group]
        baseline = median(maxima[:-1]) if len(maxima) >= 2 else max(last)
        band = max(baseline * BANDS["rss_growth_factor"], baseline + BANDS["rss_growth_slack_kb"])
        last_max = max(last)
        passed = last_max <= band and last_max <= BANDS["rss_absolute_cap_kb"]
        metrics.append(metric(
            "rss_bounded", passed,
            {"baseline_median_kb": baseline, "last_window_max_kb": last_max,
             "band_kb": int(band), "absolute_cap_kb": BANDS["rss_absolute_cap_kb"],
             "window_maxima_kb": maxima},
            {"growth_factor": BANDS["rss_growth_factor"],
             "growth_slack_kb": BANDS["rss_growth_slack_kb"]},
            {**evidence_common, "last_window": len(last)},
            reason="" if passed else "rss-returned-above-band",
        ))

    # ------------------------------------------------------------- file descriptors
    fds = windows(subject, "fds")
    if not fds or len([s for s in subject if isinstance(s.get("fds"), (int, float))]) < MIN_SAMPLES:
        metrics.append(check_metric_missing("fds_bounded", evidence_common, {
            "slack_min": BANDS["fd_slack_min"],
            "fraction": BANDS["fd_fraction"],
            "absolute_cap": BANDS["fd_absolute_cap"],
            "min_samples": MIN_SAMPLES,
        }))
    else:
        _first, last, groups = fds
        maxima = [max(group) for group in groups if group]
        baseline = median(maxima[:-1]) if len(maxima) >= 2 else max(last)
        slack = max(BANDS["fd_slack_min"], int(baseline * BANDS["fd_fraction"]))
        last_max = max(last)
        passed = last_max <= baseline + slack and last_max <= BANDS["fd_absolute_cap"]
        metrics.append(metric(
            "fds_bounded", passed,
            {"baseline_median": baseline, "last_window_max": last_max, "slack": slack,
             "absolute_cap": BANDS["fd_absolute_cap"], "window_maxima": maxima},
            {"slack_min": BANDS["fd_slack_min"], "fraction": BANDS["fd_fraction"]},
            {**evidence_common, "last_window": len(last)},
            reason="" if passed else "fds-returned-above-baseline-slack",
        ))

    # ------------------------------------------------------------- children / orphans
    final_orphans = final.get("orphans")
    last_sample = subject[-1] if subject else (samples[-1] if samples else {})
    observed_children = last_sample.get("children")
    orphaned = final_orphans if isinstance(final_orphans, int) else last_sample.get("orphans")
    passed = observed_children == 0 and (orphaned in (0, None) or orphaned == 0)
    if not subject:
        metrics.append(check_metric_missing("child_processes_zero", evidence_common, {
            "children": 0, "orphans": 0}))
    else:
        metrics.append(metric(
            "child_processes_zero", passed,
            {"final_children": observed_children, "final_orphans": orphaned},
            {"children": BANDS["child_processes"], "orphans": 0},
            {**evidence_common, "final": bool(last_sample.get("final"))},
            reason="" if passed else "live-child-or-orphan-processes-remain",
        ))

    # ---------------------------------------------------- runtime workload gauges
    queue_gauges = {
        "background_tasks_settled": "background_tasks",
        "writer_queue_zero": "writer_queue",
        "reader_queue_zero": "reader_queue",
    }
    for metric_name, key in queue_gauges.items():
        windows_value = windows(workload, key) if workload else None
        if not windows_value or len([w for w in workload if isinstance(w.get(key), (int, float))]) < 2:
            metrics.append(check_metric_missing(metric_name, evidence_common, {
                "last_window_max": 0, "min_samples": 2}))
            continue
        _first, last, groups = windows_value
        last_max = max(last)
        passed = last_max <= BANDS["queue_population"]
        metrics.append(metric(
            metric_name, passed,
            {"last_window_max": last_max, "last_window_size": len(last),
             "window_maxima": [max(g) for g in groups]},
            {"last_window_max": 0},
            evidence_common,
            reason="" if passed else f"{key} did-not-return-to-zero",
        ))

    # ------------------------------------------------------------------- WAL
    wal = windows(samples, "wal_bytes")
    if not wal or len([s for s in samples if isinstance(s.get("wal_bytes"), (int, float))]) < MIN_SAMPLES:
        metrics.append(check_metric_missing("wal_converged", evidence_common, {
            "window_slack_bytes": BANDS["wal_window_slack_bytes"],
            "absolute_bound_bytes": BANDS["wal_absolute_bound_bytes"],
            "min_samples": MIN_SAMPLES,
        }))
    else:
        _first, last, _groups = wal
        last_max, last_min = max(last), min(last)
        growth = last_max - last_min
        passed = growth <= BANDS["wal_window_slack_bytes"] and last_max <= BANDS["wal_absolute_bound_bytes"]
        metrics.append(metric(
            "wal_converged", passed,
            {"last_window_growth_bytes": growth, "last_window_max_bytes": last_max,
             "final_wal_bytes": final.get("wal_bytes", samples[-1].get("wal_bytes"))},
            {"window_slack_bytes": BANDS["wal_window_slack_bytes"],
             "absolute_bound_bytes": BANDS["wal_absolute_bound_bytes"]},
            evidence_common,
            reason="" if passed else "wal-kept-growing",
        ))

    # ------------------------------------------------------------- temp files
    last_temp = samples[-1].get("temp_files") if samples else None
    final_temp = final.get("temp_files")
    observed_temp = final_temp if isinstance(final_temp, int) else last_temp
    passed = observed_temp == 0
    if observed_temp is None:
        metrics.append(check_metric_missing("temp_files_removed", evidence_common, {"temp_files": 0}))
    else:
        metrics.append(metric(
            "temp_files_removed", passed,
            {"final_temp_files": observed_temp, "last_sample_temp_files": last_temp},
            {"temp_files": BANDS["temp_files"]},
            evidence_common,
            reason="" if passed else "temp-files-remain",
        ))

    # ------------------------------------------------------- CAS unreachable
    cas = windows(samples, "cas_unreachable")
    if not cas or len([s for s in samples if isinstance(s.get("cas_unreachable"), int)]) < MIN_SAMPLES:
        metrics.append(check_metric_missing("cas_unreachable_stable", evidence_common, {
            "slack_blobs": BANDS["cas_unreachable_slack"], "min_samples": MIN_SAMPLES}))
    else:
        first, last, groups = cas
        first_max, last_max = max(first), max(last)
        means = [sum(g) / len(g) for g in groups if g]
        monotonic = len(means) >= 3 and all(b > a for a, b in zip(means, means[1:]))
        passed = last_max <= first_max + BANDS["cas_unreachable_slack"] and not monotonic
        metrics.append(metric(
            "cas_unreachable_stable", passed,
            {"first_window_max": first_max, "last_window_max": last_max,
             "window_means": means, "monotonic_increase": monotonic,
             "final_unreachable": final.get("cas_unreachable", samples[-1].get("cas_unreachable"))},
            {"slack_blobs": BANDS["cas_unreachable_slack"], "monotonic": False},
            evidence_common,
            reason="" if passed else "cas-unreachable-population-leaked",
        ))

    # --------------------------------------------------------- latency trends
    for metric_name, key in (
        ("journal_latency_no_upward_trend", "journal_us"),
        ("index_latency_no_upward_trend", "index_us"),
    ):
        latency = windows(workload, key) if workload else None
        if not latency or len([w for w in workload if isinstance(w.get(key), (int, float))]) < 4:
            metrics.append(check_metric_missing(metric_name, evidence_common, {
                "first_window_p95_us": "<= 2.0x baseline",
                "absolute_cap_us": BANDS["latency_abs_cap_us"],
                "min_samples": 4,
            }))
            continue
        first, last, _groups = latency
        first_p95 = percentile(first, 95)
        last_p95 = percentile(last, 95)
        passed = (
            last_p95 <= max(first_p95 * BANDS["latency_factor"], first_p95 + 1000)
            and last_p95 <= BANDS["latency_abs_cap_us"]
        )
        metrics.append(metric(
            metric_name, passed,
            {"first_window_p95_us": first_p95, "last_window_p95_us": last_p95,
             "first_window_size": len(first), "last_window_size": len(last)},
            {"factor": BANDS["latency_factor"], "absolute_cap_us": BANDS["latency_abs_cap_us"]},
            evidence_common,
            reason="" if passed else f"{key}-trends-upward",
        ))

    # ------------------------------------------------------------ reconnect
    events = [w for w in workload if w.get("event") == "reconnect"]
    failures = [e for e in events if e.get("ok") is not True]
    probes = [s for s in samples if isinstance(s.get("probe_ok"), bool)]
    probe_failures = [s for s in probes if s["probe_ok"] is False]
    passed = bool(events) and not failures and not probe_failures
    reason = ""
    if not events:
        reason = "no-reconnect-events"
    elif failures or probe_failures:
        reason = "reconnect-failures-observed"
    metrics.append(metric(
        "reconnect_correct", passed,
        {"events": len(events), "failures": len(failures),
         "probe_attempts": len(probes), "probe_failures": len(probe_failures)},
        {"events": ">= 1", "failures": 0},
        evidence_common,
        reason=reason,
    ))

    # ------------------------------------------------------------- duration
    if args.target_seconds is not None:
        elapsed = args.elapsed_seconds if args.elapsed_seconds is not None else 0
        # The lane's wall budget is the upper edge of the band; the target is
        # the lower edge (a real 30-60 min / 12-24 h run, never a stub).
        lower = args.target_seconds
        passed = elapsed >= lower and (args.max_seconds is None or elapsed <= args.max_seconds)
        metrics.append(metric(
            "duration_target_met", passed,
            {"elapsed_seconds": elapsed, "target_seconds": lower,
             "max_seconds": args.max_seconds},
            {"elapsed_seconds": f">= {lower}"},
            {"lane": args.lane, "mode": args.mode},
            reason="" if passed else "campaign-duration-out-of-band",
        ))

    failed = [m for m in metrics if m["status"] != "passed"]
    status = "passed" if not failed else "failed"
    report = {
        "schema": SCHEMA,
        "status": status,
        "ok": status == "passed",
        "required": True,
        "lane": args.lane,
        "mode": args.mode,
        "commit": args.commit,
        "tree": args.tree,
        "checked_at": iso_now(),
        "metrics_required": [m["name"] for m in metrics],
        "failed_metrics": [m["name"] for m in failed],
        "bands": BANDS,
        "metrics": metrics,
    }
    return report


# ---------------------------------------------------------------- selftest


def _fake_sampler(n=20, rss_start=120_000, rss_end=121_000, fds_start=40, fds_end=42,
                  wal=1_048_576, temp_end=0, cas_first=10, cas_last=10, orphan=0):
    out = []
    for i in range(n):
        frac = i / max(1, n - 1)
        out.append({
            "t": i * 1000,
            "rss_kb": int(rss_start + (rss_end - rss_start) * frac),
            "fds": int(fds_start + (fds_end - fds_start) * frac),
            "children": 0 if frac > 0.5 else 2,
            "orphans": orphan,
            "wal_bytes": wal,
            "temp_files": 0 if frac > 0.5 else 3,
            "cas_blobs": 10,
            "cas_unreachable": int(cas_first + (cas_last - cas_first) * frac),
            "probe_ok": True,
            "final": i == n - 1,
        })
    out[-1]["temp_files"] = temp_end
    return out


def _fake_workload(n=16, latency=100, queue_end=0, reconnect_ok=True):
    rows = []
    for i in range(n):
        rows.append({
            "t": i * 1000,
            "writer_queue": 0 if i < n - 3 else queue_end,
            "reader_queue": 0 if i < n - 3 else queue_end,
            "background_tasks": 0 if i < n - 3 else queue_end,
            "journal_us": int(latency + i * latency * 0.05),
            "index_us": int(latency + i * latency * 0.05),
        })
    rows.append({"t": n * 1000, "event": "reconnect", "ok": reconnect_ok})
    rows.append({"t": n * 1000 + 1, "event": "reconnect", "ok": True})
    return rows


def run_selftest():
    failures = 0

    def check(name, report, want_status, want_metric=None):
        nonlocal failures
        ok = report["status"] == want_status
        if want_metric:
            entry = next((m for m in report["metrics"] if m["name"] == want_metric), None)
            ok = ok and entry is not None and entry["status"] == want_status
        if ok:
            print(f"selftest ok: {name}")
        else:
            print(
                f"selftest FAIL: {name} (status={report['status']} "
                f"failed={report['failed_metrics']})",
                file=sys.stderr,
            )
            failures += 1

    args = argparse.Namespace(
        samples=None, workload=None, final=None, lane="selftest", mode="churn",
        commit="0" * 40, tree="1" * 40, elapsed_seconds=None, target_seconds=None,
        max_seconds=None,
    )
    import tempfile

    with tempfile.TemporaryDirectory(prefix="soak-convergence-selftest-") as tmp:
        samples_good = os.path.join(tmp, "samples-good.jsonl")
        work_good = os.path.join(tmp, "work-good.jsonl")
        for row in _fake_sampler():
            _append_jsonl(samples_good, row)
        for row in _fake_workload():
            _append_jsonl(work_good, row)
        args.samples, args.workload = samples_good, work_good
        report = build_report(args)
        check("converged world passes", report, "passed")

        samples_bad = os.path.join(tmp, "samples-bad.jsonl")
        work_bad = os.path.join(tmp, "work-bad.jsonl")
        for row in _fake_sampler(n=20, rss_end=600_000, fds_end=600, wal=80 * 1024 * 1024,
                                 temp_end=5, cas_first=0, cas_last=40, orphan=2):
            _append_jsonl(samples_bad, row)
        bad_rows = _fake_workload(queue_end=7, reconnect_ok=False)
        bad_rows[0]["journal_us"] = 100
        bad_rows[-1]["journal_us"] = 100_000
        for row in bad_rows:
            _append_jsonl(work_bad, row)
        args.samples, args.workload = samples_bad, work_bad
        report = build_report(args)
        check("leaky world fails", report, "failed")
        for name in (
            "rss_bounded",
            "fds_bounded",
            "child_processes_zero",
            "background_tasks_settled",
            "writer_queue_zero",
            "reader_queue_zero",
            "wal_converged",
            "temp_files_removed",
            "cas_unreachable_stable",
            "reconnect_correct",
        ):
            check(f"leaky world fails {name}", report, "failed", want_metric=name)

        empty = os.path.join(tmp, "empty.jsonl")
        open(empty, "w", encoding="utf-8").close()
        args.samples, args.workload = empty, empty
        report = build_report(args)
        check("missing samples fail closed", report, "failed")
        for metric_name in ("rss_bounded", "writer_queue_zero", "reconnect_correct"):
            entry = next((m for m in report["metrics"] if m["name"] == metric_name), None)
            if entry and entry["status"] == "failed" and entry.get("reason"):
                print(f"selftest ok: missing {metric_name} is a typed failure")
            else:
                print(f"selftest FAIL: missing {metric_name} must fail typed", file=sys.stderr)
                failures += 1

        args.samples, args.workload = samples_good, work_good
        args.target_seconds = 30
        args.elapsed_seconds = 10
        report = build_report(args)
        check("short campaign fails duration band", report, "failed", want_metric="duration_target_met")
        args.elapsed_seconds = 40
        report = build_report(args)
        check("campaign in duration band passes", report, "passed")

    if failures:
        print(f"soak-convergence selftest: FAIL ({failures} case(s))", file=sys.stderr)
        return 1
    print("soak-convergence selftest: PASS (converged world, leaky world, missing-sample and duration-band matrices)")
    return 0


# -------------------------------------------------------------------- main


def main(argv):
    parser = argparse.ArgumentParser(prog="soak-convergence.py", add_help=True)
    sub = parser.add_subparsers(dest="command")

    sample = sub.add_parser("sample")
    sample.add_argument("--out", required=True)
    sample.add_argument("--pid", type=int)
    sample.add_argument("--pidfile")
    sample.add_argument("--root-pidfile")
    sample.add_argument("--data-dir")
    sample.add_argument("--probe-url")
    sample.add_argument("--interval", type=float, default=1.0)
    sample.add_argument("--duration", type=float, default=60.0)
    sample.add_argument("--max-samples", type=int, default=100_000)
    sample.add_argument("--final-scan")

    check = sub.add_parser("check")
    check.add_argument("--samples", required=True)
    check.add_argument("--workload")
    check.add_argument("--final")
    check.add_argument("--lane", default="soak")
    check.add_argument("--mode", default="")
    check.add_argument("--commit", default="")
    check.add_argument("--tree", default="")
    check.add_argument("--elapsed-seconds", type=int)
    check.add_argument("--target-seconds", type=int)
    check.add_argument("--max-seconds", type=int)
    check.add_argument("--out")

    sub.add_parser("selftest")

    args = parser.parse_args(argv)
    if args.command == "sample":
        if args.interval <= 0 or args.duration <= 0 or args.max_samples < 1:
            return 2
        if not args.pid and not args.pidfile:
            return fail("sample requires --pid or --pidfile (missing metric support is never skipped)")
        return Sampler(args).run()
    if args.command == "check":
        report = build_report(args)
        if args.out:
            _write_json_atomic(args.out, report)
        print(json.dumps(report, sort_keys=True))
        return 0 if report["ok"] else 1
    if args.command == "selftest":
        return run_selftest()
    parser.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
