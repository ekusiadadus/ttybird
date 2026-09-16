#!/usr/bin/env python3
"""Exercise reviewed handoff launch with a synthetic ``codex`` executable.

Usage: python3 scripts/handoff_smoke.py PATH_TO_TTYBIRD

The fixture never invokes a model or API. It prepares a private handoff from a
temporary Git checkout, edits the reviewed Markdown, and records exactly what
the owned PTY passes to a PATH-local ``codex`` stub.
"""

import json
import fcntl
import os
import pathlib
import pty
import select
import stat
import struct
import subprocess
import sys
import tempfile
import termios
import time


if len(sys.argv) != 2:
    raise SystemExit("usage: handoff_smoke.py PATH_TO_TTYBIRD")

BINARY = os.path.abspath(sys.argv[1])
REVIEWED_TEXT = """# Reviewed synthetic handoff

Next action: verify the private file reference.

## Selected recent conversation excerpt
SYNTHETIC_TRANSCRIPT_SENTINEL
"""
FORBIDDEN_PERMISSION_ARGS = {
    "--dangerously-bypass-approvals-and-sandbox",
    "--full-auto",
    "--ask-for-approval",
    "--approval-policy",
    "--sandbox",
    "--model",
    "--config",
}


def run(command, *, env, check=True):
    return subprocess.run(
        command,
        check=check,
        capture_output=True,
        text=True,
        env=env,
        timeout=15,
    )


def private_mode(path, expected):
    actual = stat.S_IMODE(path.stat().st_mode)
    assert actual == expected, f"{path} mode is {actual:o}, expected {expected:o}"


with tempfile.TemporaryDirectory(prefix="tb-handoff-", dir="/tmp") as root_text:
    root = pathlib.Path(root_text)
    config = root / "config"
    checkout = root / "checkout"
    bin_dir = root / "bin"
    result_path = root / "codex-result.json"
    checkout.mkdir()
    bin_dir.mkdir()

    run(["git", "-C", str(checkout), "init", "-q"], env=os.environ)
    run(
        ["git", "-C", str(checkout), "config", "user.name", "TTYbird Test"],
        env=os.environ,
    )
    run(
        [
            "git",
            "-C",
            str(checkout),
            "config",
            "user.email",
            "ttybird@example.invalid",
        ],
        env=os.environ,
    )
    note = checkout / "decision.md"
    note.write_text("Use the synthetic destination only.\n", encoding="utf-8")
    run(["git", "-C", str(checkout), "add", "decision.md"], env=os.environ)
    run(
        ["git", "-C", str(checkout), "commit", "-q", "-m", "initial"],
        env=os.environ,
    )
    run(["git", "-C", str(checkout), "branch", "-M", "main"], env=os.environ)
    checkout = checkout.resolve()

    codex = bin_dir / "codex"
    codex.write_text(
        """#!/usr/bin/env python3
import json
import os
import pathlib
import signal
import sys
import time

prefix = "Continue the task from the user-reviewed handoff file at "
separator = ". Read that file and this project's instructions first."
assert len(sys.argv) == 4 and sys.argv[1] == "--cd"
prompt = sys.argv[3]
assert prompt.startswith(prefix) and separator in prompt
draft_path = pathlib.Path(prompt[len(prefix):].split(separator, 1)[0]).resolve()
payload = {
    "argv": sys.argv[1:],
    "cwd": os.getcwd(),
    "draft_path": str(draft_path),
    "draft_text": draft_path.read_text(encoding="utf-8"),
}
result = pathlib.Path(os.environ["TTYBIRD_HANDOFF_RESULT"])
temporary = result.with_suffix(".tmp")
temporary.write_text(json.dumps(payload), encoding="utf-8")
os.replace(temporary, result)
print("HANDOFF_DESTINATION_READY", flush=True)

signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
while True:
    time.sleep(0.1)
""",
        encoding="utf-8",
    )
    codex.chmod(0o755)

    env = dict(
        os.environ,
        PATH=str(bin_dir) + os.pathsep + os.environ.get("PATH", ""),
        TTYBIRD_HANDOFF_RESULT=str(result_path),
        CODEX_HOME=str(root / "empty-codex"),
        CLAUDE_CONFIG_DIR=str(root / "empty-claude"),
    )
    base = [BINARY, "--config-dir", str(config), "--local"]
    session_id = None
    dashboard = None

    def prepare_bundle():
        completed = run(
            base
            + [
                "--json",
                "handoff",
                "prepare",
                "--cwd",
                str(checkout),
                "--note",
                "decision.md",
            ],
            env=env,
        )
        payload = json.loads(completed.stdout)
        assert payload == {
            "bundle": payload["bundle"],
            "destination": "codex",
            "started": False,
        }
        return pathlib.Path(payload["bundle"]).resolve()

    try:
        bundle = prepare_bundle()
        private_mode(config / "handoffs", 0o700)
        private_mode(bundle, 0o700)
        private_mode(bundle / "manifest.json", 0o600)
        private_mode(bundle / "draft.md", 0o600)
        prepared = (bundle / "draft.md").read_text(encoding="utf-8")
        assert "Use the synthetic destination only." in prepared
        assert "Selected recent conversation excerpt" not in prepared

        (bundle / "draft.md").write_text(REVIEWED_TEXT, encoding="utf-8")
        launched = run(
            base
            + [
                "--json",
                "handoff",
                "start",
                str(bundle),
                "--yes",
                "--detach",
            ],
            env=env,
        )
        session = json.loads(launched.stdout)
        session_id = session["id"]

        deadline = time.monotonic() + 10
        while not result_path.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        assert result_path.exists(), "synthetic codex did not record its launch"
        observed = json.loads(result_path.read_text(encoding="utf-8"))
        assert pathlib.Path(observed["cwd"]).resolve() == checkout
        assert observed["argv"][:2] == ["--cd", str(checkout)]
        assert len(observed["argv"]) == 3, observed["argv"]
        assert not FORBIDDEN_PERMISSION_ARGS.intersection(observed["argv"])
        assert "SYNTHETIC_TRANSCRIPT_SENTINEL" not in "\n".join(observed["argv"])
        assert observed["draft_text"] == REVIEWED_TEXT

        destination_draft = pathlib.Path(observed["draft_path"])
        destination_bundle = destination_draft.parent
        assert destination_draft.name == "draft.md"
        assert destination_bundle.parent == (config / "handoffs").resolve()
        private_mode(destination_bundle, 0o700)
        private_mode(destination_draft, 0o600)
        private_mode(destination_bundle / "manifest.json", 0o600)
        private_mode(destination_bundle / "destination.json", 0o600)
        destination = json.loads(
            (destination_bundle / "destination.json").read_text(encoding="utf-8")
        )
        assert destination["id"] == session_id

        bundle_count = len(list((config / "handoffs").iterdir()))
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 160, 0, 0))

        def setup_terminal():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        dashboard_process = subprocess.Popen(
            base + ["attach", session_id],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=dict(env, TERM="xterm-256color"),
            preexec_fn=setup_terminal,
            close_fds=True,
        )
        os.close(slave)
        slave = None
        dashboard_output = bytearray()
        dashboard = (dashboard_process, master, dashboard_output)

        def drain_dashboard(seconds):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if not select.select([master], [], [], 0.05)[0]:
                    continue
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                if not data:
                    break
                dashboard_output.extend(data)

        def wait_dashboard(marker, seconds=10):
            deadline = time.monotonic() + seconds
            while marker not in dashboard_output and time.monotonic() < deadline:
                drain_dashboard(0.1)
                if dashboard_process.poll() is not None:
                    break
            assert marker in dashboard_output, (
                f"dashboard missing {marker!r}; exit={dashboard_process.poll()}; "
                f"tail={bytes(dashboard_output[-1200:])!r}"
            )

        wait_dashboard(b"HANDOFF_DESTINATION_READY")
        dashboard_output.clear()
        os.write(master, b"H")
        wait_dashboard(b"REVIEW")
        dashboard_output.clear()
        os.write(master, b"eS")
        drain_dashboard(0.5)
        sessions_during_edit = json.loads(
            run(base + ["--json", "sessions"], env=env).stdout
        )
        assert sum(not item["ended"] for item in sessions_during_edit) == 1, (
            "uppercase S in edit mode started another session"
        )
        os.write(master, b"YNTHETIC_EDIT")
        drain_dashboard(0.8)
        os.write(master, b"\x1b")
        drain_dashboard(0.4)
        os.write(master, b"\x1b")
        drain_dashboard(0.4)
        dashboard_output.clear()
        os.write(master, b"N")
        wait_dashboard(b"No open observed events")
        os.write(master, b"\x1b")
        drain_dashboard(0.4)
        os.write(master, b"q")
        deadline = time.monotonic() + 5
        while dashboard_process.poll() is None and time.monotonic() < deadline:
            drain_dashboard(0.05)
        assert dashboard_process.wait(timeout=2) == 0
        os.close(master)
        dashboard = None
        assert len(list((config / "handoffs").iterdir())) == bundle_count, (
            "cancelled TUI handoff persisted a bundle"
        )
        sessions_after_tui = json.loads(
            run(base + ["--json", "sessions"], env=env).stdout
        )
        assert not next(
            item for item in sessions_after_tui if item["id"] == session_id
        )["ended"], "closing the dashboard stopped the owned destination"

        stopped = run(base + ["stop", session_id], env=env)
        assert f"Stopped {session_id}" in stopped.stdout
        deadline = time.monotonic() + 5
        socket_path = config / "managed" / f"{session_id}.sock"
        while socket_path.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        assert not socket_path.exists(), "stop left the owned-session socket"
        sessions = json.loads(run(base + ["--json", "sessions"], env=env).stdout)
        assert next(item for item in sessions if item["id"] == session_id)["ended"]
        session_id = None

        stale_bundle = prepare_bundle()
        run(
            ["git", "-C", str(checkout), "switch", "-q", "-c", "drifted"],
            env=env,
        )
        rejected = run(
            base
            + [
                "handoff",
                "start",
                str(stale_bundle),
                "--yes",
                "--detach",
            ],
            env=env,
            check=False,
        )
        assert rejected.returncode != 0, "branch drift unexpectedly launched a handoff"
        assert "prepare and review a new handoff" in rejected.stderr

        print(
            json.dumps(
                {
                    "synthetic_codex_only": True,
                    "reviewed_draft_exact": True,
                    "cwd_verified": True,
                    "private_reference": True,
                    "content_not_in_argv": True,
                    "permission_args_not_copied": True,
                    "tui_review_edit_cancel": True,
                    "tui_uppercase_s_is_text": True,
                    "empty_observed_inbox": True,
                    "dashboard_detach_preserves_destination": True,
                    "stop_cleanup": True,
                    "branch_drift_rejected": True,
                }
            )
        )
    finally:
        if dashboard is not None:
            process, master, _ = dashboard
            try:
                os.close(master)
            except OSError:
                pass
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        if session_id is not None:
            run(base + ["stop", session_id], env=env, check=False)
