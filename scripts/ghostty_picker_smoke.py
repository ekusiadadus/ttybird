"""Exercise explicit Ghostty pane selection without focusing the real app.

Usage: python3 scripts/ghostty_picker_smoke.py PATH_TO_TTYBIRD

The discovered coding CLI is a synthetic Node process with Gemini's documented
package path. A PATH-scoped osascript stub returns synthetic surface metadata
and records only the UUID passed to the focus command.
"""

import fcntl
import json
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


FIRST_UUID = "11111111-1111-4111-8111-111111111111"
SECOND_UUID = "22222222-2222-4222-8222-222222222222"


def read_bindings(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as file:
        return json.load(file)


def terminate(process):
    if process is None or process.poll() is not None:
        return
    try:
        process.terminate()
        process.wait(timeout=5)
    except (ProcessLookupError, PermissionError):
        pass
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: ghostty_picker_smoke.py PATH_TO_TTYBIRD")
    binary = os.path.abspath(sys.argv[1])
    node = shutil.which("node")
    if node is None:
        raise RuntimeError("node is required for the synthetic Gemini fixture")

    with tempfile.TemporaryDirectory(prefix="ttybird-ghostty-picker-") as temp_root:
        # macOS reports process cwd through /private/var while tempfile uses the
        # /var symlink. Keep the fixture and picker metadata on one identity.
        root = os.path.realpath(temp_root)
        fixture = dashboard = None
        master = slave = None
        output = bytearray()
        result_path = os.path.join(root, "terminal-result.json")
        pid_path = os.path.join(root, "dashboard.pid")
        bindings_path = os.path.join(root, "bindings.json")
        focus_path = os.path.join(root, "focused-uuid")
        try:
            for directory in ("empty-codex", "empty-claude", "fake-bin"):
                os.makedirs(os.path.join(root, directory))

            entry = os.path.join(
                root, "node_modules", "@google", "gemini-cli", "dist", "index.js"
            )
            os.makedirs(os.path.dirname(entry))
            with open(entry, "w", encoding="utf-8") as file:
                file.write("setInterval(() => {}, 90000);\n")
            fixture = subprocess.Popen(
                [node, entry],
                cwd=root,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                close_fds=True,
            )

            stub_path = os.path.join(root, "fake-bin", "osascript")
            stub = f'''#!/usr/bin/env python3
import json
import sys

if "JavaScript" in sys.argv:
    print(json.dumps([
        {{"id": {FIRST_UUID!r}, "cwd": {root!r}, "title": "synthetic matching pane"}},
        {{"id": {SECOND_UUID!r}, "cwd": "/synthetic/other", "title": "synthetic second pane"}},
    ]))
    raise SystemExit(0)
if "AppleScript" in sys.argv and sys.argv[-1] in ({FIRST_UUID!r}, {SECOND_UUID!r}):
    with open({focus_path!r}, "a", encoding="utf-8") as file:
        file.write(sys.argv[-1] + "\\n")
    raise SystemExit(0)
raise SystemExit(64)
'''
            with open(stub_path, "w", encoding="utf-8") as file:
                file.write(stub)
            os.chmod(stub_path, 0o755)

            env = dict(
                os.environ,
                TERM="xterm-256color",
                CODEX_HOME=os.path.join(root, "empty-codex"),
                CLAUDE_CONFIG_DIR=os.path.join(root, "empty-claude"),
                CLAUDE_HOME=os.path.join(root, "empty-claude"),
                PATH=os.path.join(root, "fake-bin") + os.pathsep + os.environ["PATH"],
            )
            env.pop("TMUX", None)

            def discover(process):
                expected = None
                deadline = time.monotonic() + 10
                while expected is None and time.monotonic() < deadline:
                    inventory = json.loads(
                        subprocess.check_output(
                            [
                                binary,
                                "--local",
                                "--config-dir",
                                root,
                                "--json",
                            ],
                            env=env,
                            timeout=5,
                        )
                    )
                    matches = [
                        row
                        for snapshot in inventory
                        for row in snapshot["sessions"]
                        if row.get("provider") == "gemini" and row.get("cwd") == root
                    ]
                    if len(matches) == 1:
                        expected = matches[0]
                        break
                    time.sleep(0.1)
                assert expected is not None, "synthetic Gemini process was not discovered"
                assert expected.get("pid") == process.pid, (
                    "collector attributed the wrong process"
                )
                assert expected.get("process_started_at"), (
                    "process start identity is missing"
                )
                assert expected.get("target") is None, (
                    "fixture unexpectedly began with a binding"
                )
                return expected

            expected = discover(fixture)

            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 140, 0, 0))

            def setup():
                os.setsid()
                fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

            wrapper = '''import json,subprocess,sys,termios
before=termios.tcgetattr(0)
p=subprocess.Popen(sys.argv[3:])
open(sys.argv[1],'w').write(str(p.pid))
code=p.wait()
json.dump({'exit':code,'restored':before==termios.tcgetattr(0)},open(sys.argv[2],'w'))
sys.exit(code)
'''
            dashboard = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    wrapper,
                    pid_path,
                    result_path,
                    binary,
                    "--local",
                    "--config-dir",
                    root,
                ],
                stdin=slave,
                stdout=slave,
                stderr=slave,
                env=env,
                preexec_fn=setup,
                close_fds=True,
            )

            def drain(seconds):
                end = time.monotonic() + seconds
                while time.monotonic() < end:
                    if not select.select([master], [], [], 0.04)[0]:
                        continue
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        break
                    if not data:
                        break
                    output.extend(data)

            def wait_for(marker, seconds=12):
                end = time.monotonic() + seconds
                while marker not in output and time.monotonic() < end:
                    if dashboard.poll() is not None:
                        break
                    drain(0.1)
                assert marker in output, "expected dashboard state was not rendered"

            wait_for(b"TTYbird")
            os.write(master, ("/" + os.path.basename(root)).encode())
            drain(0.3)
            os.write(master, b"\r")
            drain(0.2)

            output.clear()
            os.write(master, b"\r")
            wait_for(b"Choose Ghostty pane")
            assert read_bindings(bindings_path) == [], "cwd match bound without explicit choice"
            assert not os.path.exists(focus_path), "picker focused before explicit confirmation"

            output.clear()
            os.write(master, b"\x1b")
            drain(0.3)
            assert read_bindings(bindings_path) == [], "cancel changed bindings"
            assert not os.path.exists(focus_path), "cancel invoked focus"

            # The chooser freezes the selected process identity while it is
            # open. Terminating that exact fixture before confirmation must be
            # rejected by ttybird's fresh collection, before any binding write
            # or focus transport invocation.
            output.clear()
            os.write(master, b"\r")
            wait_for(b"Choose Ghostty pane")
            terminate(fixture)
            output.clear()
            os.write(master, b"\r")
            wait_for(b"Cannot bind pane")
            assert dashboard.poll() is None, "stale picker confirmation exited the dashboard"
            assert not os.path.exists(bindings_path), "stale picker wrote a bindings file"
            assert not os.path.exists(focus_path), "stale picker invoked focus"

            output.clear()
            os.write(master, b"q")
            deadline = time.monotonic() + 10
            while dashboard.poll() is None and time.monotonic() < deadline:
                drain(0.05)
            assert dashboard.poll() == 0, "dashboard did not exit after stale rejection"
            drain(0.1)
            stale_terminal_result = json.load(open(result_path, encoding="utf-8"))
            assert stale_terminal_result == {"exit": 0, "restored": True}, (
                stale_terminal_result
            )
            os.close(master)
            os.close(slave)
            master = slave = None

            fixture = subprocess.Popen(
                [node, entry],
                cwd=root,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                close_fds=True,
            )
            expected = discover(fixture)

            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 140, 0, 0))
            dashboard = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    wrapper,
                    pid_path,
                    result_path,
                    binary,
                    "--local",
                    "--config-dir",
                    root,
                ],
                stdin=slave,
                stdout=slave,
                stderr=slave,
                env=env,
                preexec_fn=setup,
                close_fds=True,
            )
            output.clear()
            wait_for(b"TTYbird")
            os.write(master, ("/" + os.path.basename(root)).encode())
            drain(0.3)
            os.write(master, b"\r")
            drain(0.2)

            output.clear()
            os.write(master, b"\r")
            wait_for(b"Choose Ghostty pane")
            os.write(master, b"\x1b[B\r")

            deadline = time.monotonic() + 10
            while dashboard.poll() is None and time.monotonic() < deadline:
                drain(0.05)
            assert dashboard.poll() == 0, "dashboard did not exit cleanly after explicit choice"
            drain(0.1)

            terminal_result = json.load(open(result_path, encoding="utf-8"))
            assert terminal_result == {"exit": 0, "restored": True}, terminal_result
            assert b"\x1b[?1049l" in output, "alternate screen was not restored"

            bindings = read_bindings(bindings_path)
            assert len(bindings) == 1, "explicit choice did not create exactly one binding"
            binding = bindings[0]
            assert binding["session_id"] == expected["id"], "binding used the wrong session"
            assert binding["host"] == expected["host"], "binding used the wrong host"
            assert binding["pid"] == expected["pid"], "binding used the wrong PID"
            assert (
                binding["process_started_at"] == expected["process_started_at"]
            ), "binding used the wrong process start time"
            assert binding["target"] == {
                "kind": "ghostty",
                "terminal_id": SECOND_UUID,
            }, "binding did not preserve the explicit second UUID"
            with open(focus_path, encoding="utf-8") as file:
                focused = file.read().splitlines()
            assert focused == [SECOND_UUID], "focus did not receive exactly the chosen UUID"

            print(
                json.dumps(
                    {
                        "provider": "gemini",
                        "synthetic_process_only": True,
                        "cancel_preserved_empty_bindings": True,
                        "stale_picker_rejected_without_binding": True,
                        "stale_picker_focus_calls": 0,
                        "explicit_second_uuid_bound": True,
                        "fake_focus_called_once": True,
                        "terminal_restored": True,
                        "exit": 0,
                    }
                )
            )
        finally:
            if dashboard is not None and dashboard.poll() is None and os.path.exists(pid_path):
                try:
                    os.kill(int(open(pid_path, encoding="utf-8").read()), signal.SIGKILL)
                except (ProcessLookupError, PermissionError, ValueError):
                    pass
            terminate(dashboard)
            for fd in (master, slave):
                if fd is not None:
                    os.close(fd)
            terminate(fixture)


if __name__ == "__main__":
    main()
