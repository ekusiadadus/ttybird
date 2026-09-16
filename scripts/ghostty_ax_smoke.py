#!/usr/bin/env python3
"""Explicit manual Ghostty integration test; opens synthetic windows, then closes them.

Uses existing AX permission without prompting. Does not capture existing terminal
bodies. Evidence has timestamps, marker booleans and counts, never screen bodies.
Usage: python3 scripts/ghostty_ax_smoke.py EVIDENCE_DIRECTORY
"""
import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def main():
    evidence = Path(sys.argv[1]).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    repo = Path(__file__).resolve().parent.parent
    attempt = datetime.datetime.now(datetime.timezone.utc).strftime("attempt-%Y%m%dT%H%M%SZ")
    env = os.environ.copy()
    env.pop("SDKROOT", None)
    env["DEVELOPER_DIR"] = "/Applications/Xcode.app/Contents/Developer"
    with tempfile.TemporaryDirectory(prefix="ttybird-ax-fixture-") as name:
        root = Path(name)
        # Keep Apple's direct-command parsing unambiguous in this manual fixture.
        command = f"{sys.executable} {repo / 'scripts/ghostty_ax_fixture.py'}"
        if any(c in command + name for c in ['"', "\n", "\\"]) or " " in str(repo) or " " in sys.executable:
            raise ValueError("probe paths must not contain spaces or shell escapes")
        with (evidence / f"{attempt}.jsonl").open("w") as output, (evidence / f"{attempt}.stderr").open("w") as errors:
            output.write(json.dumps({"at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "checkpoint": "probe-sources", "status": "recorded", "sha256": {
                    name: hashlib.sha256((repo / "scripts" / name).read_bytes()).hexdigest()
                    for name in ["ghostty_ax_smoke.py", "ghostty_ax_probe.swift", "ghostty_ax_fixture.py"]}}) + "\n")
            output.flush()
            try:
                result = subprocess.run(
                    ["/usr/bin/swift", str(repo / "scripts/ghostty_ax_probe.swift"), command, name],
                    env=env, stdout=output, stderr=errors, timeout=80,
                )
                exit_code = result.returncode
            except subprocess.TimeoutExpired:
                exit_code = 124
                output.write(json.dumps({"at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    "checkpoint": "ERROR", "status": "failed", "reason": "probe exceeded 80 seconds"}) + "\n")
            finally:
                (root / "phase").write_text("EXIT")
                time.sleep(2)
            output.flush()
            # Recover even after a probe exception using only UUIDs it created.
            owned_ids = set()
            for line in (evidence / f"{attempt}.jsonl").read_text().splitlines():
                row = json.loads(line)
                for key in ["terminal_id", "second_terminal_id", "split_terminal_id", "replacement_terminal_id"]:
                    if key in row:
                        owned_ids.add(row[key])
            close_failures = []
            for ident in sorted(owned_ids):
                body = f'tell application "Ghostty"\nif exists terminal id "{ident}" then close terminal id "{ident}"\nend tell'
                try:
                    closed = subprocess.run(["/usr/bin/osascript", "-e", body], capture_output=True, timeout=5)
                    if closed.returncode:
                        close_failures.append(ident)
                except subprocess.TimeoutExpired:
                    close_failures.append(ident)
            alive = []
            for pid_file in root.glob("pid-*"):
                pid = int(pid_file.read_text())
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    continue
                alive.append(pid)
            row = {
                "at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "checkpoint": "C08-processes", "status": "passed" if not alive else "failed",
                "fixture_pids_remaining": alive, "probe_exit_code": exit_code,
                "cleanup_close_failures": close_failures,
                "initial_terminal_rows": {p.name: int(p.read_text()) for p in root.glob("initial-rows-*")},
            }
            output.write(json.dumps(row, sort_keys=True) + "\n")
    records = [json.loads(line) for line in (evidence / f"{attempt}.jsonl").read_text().splitlines()]
    with (evidence / "timeline.jsonl").open("a") as timeline:
        for row in records:
            timeline.write(json.dumps({**row, "attempt": attempt}, ensure_ascii=False) + "\n")
    print(json.dumps({"attempt": attempt, "exit": exit_code, "checkpoints": len(records), "remaining_fixture_pids": alive}))
    return exit_code or bool(alive) or bool(close_failures)


if __name__ == "__main__":
    raise SystemExit(main())
