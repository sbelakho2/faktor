#!/usr/bin/env python3
"""Byte-for-byte parity port of scripts/certification/lane-marker.mjs.

Why a port: the trusted/nightly linux lanes run on digest-pinned images that
deliberately carry python3 but NOT node (rust:1.98.0, ghcr.io/.../faktor-ci),
while the node lanes (node:24, playwright) carry both. The lane-facing entry
point scripts/certification/lane-marker.sh picks node when present and falls
back to this file, so every lane produces the same marker and the same
HMAC-SHA256 auth with one JSON/HMAC authority. `evidence.mjs selftest` proves
the two implementations agree byte-for-byte.

See lane-marker.mjs for the contract; keep both files in lockstep.
"""

import base64
import hashlib
import hmac
import json
import os
import platform
import re
import subprocess
import sys
from datetime import datetime, timezone

MARKER_SCHEMA = "faktor-woodpecker-lane/v2"


def sha256_hex(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def canonical_json(value):
    if isinstance(value, list):
        return "[" + ",".join(canonical_json(item) for item in value) + "]"
    if isinstance(value, dict):
        return (
            "{"
            + ",".join(
                json.dumps(key, ensure_ascii=False) + ":" + canonical_json(value[key])
                for key in sorted(value)
            )
            + "}"
        )
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    return json.dumps(value, ensure_ascii=False)


def artifact_lines(artifacts):
    return "".join(
        "{}\t{}\n".format(item["sha256"], item["path"])
        for item in sorted(artifacts, key=lambda item: item["path"])
    )


def artifact_digest(artifacts):
    return "sha256:" + hashlib.sha256(artifact_lines(artifacts).encode("utf-8")).hexdigest()


def hash_file(path):
    with open(path, "rb") as handle:
        return "sha256:" + hashlib.sha256(handle.read()).hexdigest()


def marker_auth_payload(record):
    unsigned = {key: value for key, value in record.items() if key != "auth"}
    return canonical_json(unsigned)


def compute_marker_auth(record, token):
    mac = hmac.new(
        str(token).encode("utf-8"),
        marker_auth_payload(record).encode("utf-8"),
        hashlib.sha256,
    ).hexdigest()
    return "hmac-sha256:" + mac


def normalize_lane(lane):
    return re.sub(r"[^a-z0-9]+", "_", str(lane).lower()).strip("_")


def lane_token_env_names(lane):
    normalized = normalize_lane(lane)
    return [
        "faktor_lane_token_" + normalized,
        "FAKTOR_LANE_TOKEN_" + normalized.upper(),
    ]


def lane_token_from_env(lane, env=None):
    env = os.environ if env is None else env
    for name in lane_token_env_names(lane):
        value = env.get(name)
        if value:
            return value
    return ""


def iso_now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def iso_from_epoch(raw):
    if not str(raw or "").isdigit():
        return None
    try:
        return datetime.fromtimestamp(int(raw), timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    except (OverflowError, OSError, ValueError):
        return None


def default_started_at():
    raw = os.environ.get("CI_PIPELINE_STARTED", "")
    as_epoch = iso_from_epoch(raw)
    if as_epoch:
        return as_epoch
    if raw:
        try:
            parsed = datetime.fromisoformat(raw.replace("Z", "+00:00"))
            if parsed.tzinfo is None:
                parsed = parsed.replace(tzinfo=timezone.utc)
            return parsed.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        except ValueError:
            pass
    return iso_now()


def default_runner_os():
    system = platform.system().lower()
    return {"darwin": "darwin", "windows": "windows", "linux": "linux"}.get(system, "unknown")


def default_runner_arch():
    machine = platform.machine().lower()
    return {
        "x86_64": "x86_64",
        "amd64": "x86_64",
        "aarch64": "arm64",
        "arm64": "arm64",
        "i386": "i686",
        "i686": "i686",
    }.get(machine, machine or "unknown")


def git(args, cwd):
    return subprocess.check_output(["git", "-C", cwd, *args], text=True).strip()


def worktree_clean(cwd):
    """True when the tracked working tree is clean (git diff --exit-code and
    git diff --cached --exit-code both succeed). Any git failure is NOT clean:
    the verifier fails closed instead of certifying unproven bytes."""
    for args in (
        ["diff", "--exit-code", "--quiet"],
        ["diff", "--cached", "--exit-code", "--quiet"],
    ):
        try:
            subprocess.run(
                ["git", "-C", cwd, *args],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except Exception:  # noqa: BLE001 - any git failure means not proven clean
            return False
    return True


def flag_value(args, name, fallback=""):
    if name not in args:
        return fallback
    index = args.index(name)
    if index + 1 >= len(args) or args[index + 1].startswith("--"):
        return ""
    return args[index + 1]


def flag_values(args, name):
    values = []
    for index, item in enumerate(args):
        if item == name and index + 1 < len(args) and not args[index + 1].startswith("--"):
            values.append(args[index + 1])
    return values


def read_stdin():
    if sys.stdin.isatty():
        return ""
    return sys.stdin.buffer.read().decode("utf-8")


def generate_marker(options):
    lane = options.get("lane", "")
    status = options.get("status", "passed")
    if not lane:
        raise ValueError("--lane is required")
    if status not in ("passed", "failed", "skipped"):
        raise ValueError("--status '{}' is not passed|failed|skipped".format(status))
    commands_text = options.get("commandsText", "")
    if not isinstance(commands_text, str) or not commands_text:
        raise ValueError(
            "no commands text: pipe the lane commands on stdin or pass --commands/--commands-file"
        )
    record = {
        "schema": MARKER_SCHEMA,
        "lane": lane,
        "status": status,
    }
    if options.get("reasonProvided"):
        record["reason"] = options.get("reason", "")
    record.update(
        {
            "commit": options.get("commit"),
            "tree": options.get("tree"),
            "runner": options.get("runner"),
            "started_at": options.get("startedAt"),
            "finished_at": options.get("finishedAt"),
            "commands_b64": base64.b64encode(commands_text.encode("utf-8")).decode("ascii"),
            "commands_digest": "sha256:" + sha256_hex(commands_text),
            "artifacts": options.get("artifacts", []),
            "artifact_digest": artifact_digest(options.get("artifacts", [])),
            "clean": bool(options.get("clean", True)),
        }
    )
    token = options.get("token", "")
    if token:
        record["auth"] = compute_marker_auth(record, token)
    return record


def usage():
    print(
        "usage: lane-marker.py write --lane ID [--status passed|failed|skipped]\n"
        "  [--reason TEXT] [--artifact PATH]... [--optional-artifact PATH]...\n"
        "  [--commands TEXT | --commands-file FILE] [--out FILE] [--cwd DIR]\n"
        "  [--commit SHA] [--tree SHA] [--run-id ID] [--started-at ISO] [--finished-at ISO]\n"
        "  [--runner-os OS] [--runner-arch ARCH] [--runner-ci CI]\n\n"
        "Commands default to stdin (pipe the lane's CMDS heredoc into write).\n"
        "Token: lane's own CI secret env (faktor_lane_token_<lane>, uppercase accepted).",
        file=sys.stderr,
    )


def write_command(args):
    cwd = os.path.realpath(flag_value(args, "--cwd", "."))
    lane = flag_value(args, "--lane")
    if not lane:
        usage()
        return 2
    commands_file = flag_value(args, "--commands-file")
    if commands_file:
        with open(os.path.join(cwd, commands_file), encoding="utf-8") as handle:
            commands_text = handle.read()
    elif "--commands" in args:
        commands_text = flag_value(args, "--commands")
    else:
        commands_text = read_stdin()
    artifacts = []
    for path in flag_values(args, "--artifact"):
        full = os.path.join(cwd, path)
        if not os.path.isfile(full):
            raise ValueError("--artifact {} is not a file in {}".format(path, cwd))
        artifacts.append({"path": path, "sha256": hash_file(full)})
    for path in flag_values(args, "--optional-artifact"):
        full = os.path.join(cwd, path)
        if os.path.isfile(full):
            artifacts.append({"path": path, "sha256": hash_file(full)})
    commit = flag_value(args, "--commit") or os.environ.get("CI_COMMIT_SHA", "")
    if not commit:
        commit = git(["rev-parse", "HEAD"], cwd)
    tree = flag_value(args, "--tree")
    if not tree:
        try:
            tree = git(["rev-parse", "HEAD^{tree}"], cwd)
        except Exception:  # noqa: BLE001 - any git failure degrades to 'unknown' like the JS helper
            tree = "unknown"
    run_id = flag_value(args, "--run-id") or os.environ.get("CI_PIPELINE_NUMBER") or "0"
    record = generate_marker(
        {
            "lane": lane,
            "status": flag_value(args, "--status", "passed"),
            "reasonProvided": "--reason" in args,
            "reason": flag_value(args, "--reason"),
            "commandsText": commands_text,
            "artifacts": artifacts,
            "commit": commit,
            "tree": tree,
            "startedAt": flag_value(args, "--started-at") or default_started_at(),
            "finishedAt": flag_value(args, "--finished-at") or iso_now(),
            "clean": worktree_clean(cwd),
            "runner": {
                "os": flag_value(args, "--runner-os") or default_runner_os(),
                "arch": flag_value(args, "--runner-arch") or default_runner_arch(),
                "ci": flag_value(args, "--runner-ci")
                or ("woodpecker" if os.environ.get("CI") else "local"),
                "run_id": run_id,
            },
            "token": lane_token_from_env(lane),
        }
    )
    out = os.path.realpath(
        os.path.join(cwd, flag_value(args, "--out", "target/certification/lanes/{}.json".format(lane)))
    )
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n")
    print(
        "lane-marker: {} lane={} status={} auth={}".format(
            out, lane, record["status"], "hmac-sha256" if record.get("auth") else "none"
        )
    )
    return 0


def main(argv):
    if not argv or argv[0] in ("-h", "--help", "help"):
        usage()
        return 0 if argv else 2
    command, args = argv[0], argv[1:]
    try:
        if command == "write":
            return write_command(args)
        print("lane-marker: unknown command '{}'".format(command), file=sys.stderr)
        usage()
        return 2
    except Exception as error:  # noqa: BLE001 - CLI boundary: report and exit 1
        print("lane-marker: {}".format(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
