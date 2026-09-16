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


def terminate_owned_group(process):
    """Stop every process in the dashboard's fixture-only session."""
    if process is None or process.returncode is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        pass
    # Keep the leader unreaped until after the final group signal, preventing
    # its PID from being reused as an unrelated process-group ID.
    time.sleep(0.1)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        # A stopped fixture wrapper may not consume the group signal promptly;
        # kill and reap the owned leader directly without touching other jobs.
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
        bindings_path = os.path.join(root, "bindings.json")
        focus_path = os.path.join(root, "focused-uuid")
        fail_focus_path = os.path.join(root, "fail-focus")
        export_path = os.path.join(root, "export-calls")
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
import os
import sys

if "JavaScript" in sys.argv and any("write_screen_file:copy,vt" in arg for arg in sys.argv):
    with open({export_path!r}, "a") as file:
        file.write(sys.argv[-3] + "\\n")
    print(json.dumps({{"ok": False, "error": "terminal_not_found"}}))
    raise SystemExit(0)
if "JavaScript" in sys.argv:
    print(json.dumps([
        {{"id": {FIRST_UUID!r}, "cwd": {root!r}, "title": "synthetic matching pane"}},
        {{"id": {SECOND_UUID!r}, "cwd": "/synthetic/other", "title": "synthetic second pane"}},
    ]))
    raise SystemExit(0)
if "AppleScript" in sys.argv and sys.argv[-1] in ({FIRST_UUID!r}, {SECOND_UUID!r}):
    if os.path.exists({fail_focus_path!r}):
        print("TTYBIRD_GHOSTTY_ERROR|-1712|synthetic focus failure")
        raise SystemExit(0)
    with open({focus_path!r}, "a", encoding="utf-8") as file:
        file.write(sys.argv[-1] + "\\n")
    print("TTYBIRD_GHOSTTY_OK")
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

            wrapper = '''import fcntl,json,os,signal,subprocess,sys,termios
state=getattr(termios,'PENDIN',0)
mutable=0
for name in ('O_APPEND','O_ASYNC','O_SYNC','O_DSYNC','O_NONBLOCK'): mutable|=getattr(os,name,0)
mutable|=os.O_ACCMODE
def comparable(value): return value[:3]+[value[3]&~state]+value[4:]
before=termios.tcgetattr(0)
flags=fcntl.fcntl(0,fcntl.F_GETFL)
p=None
def forward(signum,frame):
    if p is not None:
        try: p.send_signal(signum)
        except ProcessLookupError: pass
for signum in (signal.SIGHUP,signal.SIGTERM): signal.signal(signum,forward)
try:
    p=subprocess.Popen(sys.argv[2:])
    code=p.wait()
finally:
    if p is not None and p.poll() is None:
        p.kill();p.wait()
restored=comparable(before)==comparable(termios.tcgetattr(0)) and flags&mutable==fcntl.fcntl(0,fcntl.F_GETFL)&mutable
json.dump({'exit':code,'restored':restored},open(sys.argv[1],'w'))
sys.exit(code)
'''
            dashboard = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    wrapper,
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
                assert marker in output, (
                    f"expected dashboard marker {marker!r} was not rendered; "
                    f"tail={bytes(output[-4000:])!r}"
                )

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
            terminate_owned_group(dashboard)
            dashboard = None

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
            # A reported Ghostty/Automation failure must roll back the new
            # mapping without exiting and expose its bounded error detail.
            open(fail_focus_path, "w").close()
            output.clear()
            os.write(master, b"\x1b[B\r")
            # The terminal renderer may split changed text into cursor-addressed
            # runs. Wait for the distinctive failure suffix, not the whole line.
            wait_for(b"synthetic focus failure")
            assert dashboard.poll() is None, "failed focus closed the dashboard"
            assert read_bindings(bindings_path) == [], "failed focus retained the new binding"
            os.unlink(fail_focus_path)
            output.clear()
            os.write(master, b"\r")
            wait_for(b"Choose Ghostty pane")
            os.write(master, b"\x1b[B\r")
            wait_for(b"no longer exists")
            assert dashboard.poll() is None, "focus closed the dashboard"
            assert b"\x1b[?1049l" not in output, "focus left the alternate screen"

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

            # The newly mapped selection captures without p or confirmation.
            wait_for(b"no longer exists")
            drain(2.3)
            with open(export_path) as file:
                assert file.read().splitlines() == [SECOND_UUID], "mapped selection did not export exactly once"
            assert b"Press r to capture" not in output, "preview added an extra prompt"
            output.clear()
            os.write(master, b"r")
            drain(2.3)
            with open(export_path) as file:
                assert file.read().splitlines() == [SECOND_UUID, SECOND_UUID], "r did not refresh once"
            assert dashboard.poll() is None, "snapshot failure closed the dashboard"
            with open(focus_path, encoding="utf-8") as file:
                assert file.read().splitlines() == [SECOND_UUID], "snapshot focused Ghostty"
            os.write(master, b"p")
            drain(0.2)
            # Closing preview stays closed through routine collection refresh.
            os.write(master, b"r")
            drain(2.3)
            with open(export_path) as file:
                assert file.read().splitlines() == [SECOND_UUID, SECOND_UUID]
            # p remains available to reopen and capture the same selection.
            os.write(master, b"p")
            drain(2.3)
            with open(export_path) as file:
                assert file.read().splitlines() == [SECOND_UUID] * 3
            os.write(master, b"p")
            drain(0.2)

            # Explicit relink is also cancellable. Opening the chooser from an
            # existing Ghostty mapping must not replace or focus that mapping.
            output.clear()
            os.write(master, b"g")
            wait_for(b"Choose Ghostty pane")
            os.write(master, b"\x1b")
            # The preceding notice shares most of this row; ratatui leaves the
            # unchanged "c" cell in place and only emits the changed suffix.
            wait_for(b"hanged.")
            assert b"no binding " in output
            assert read_bindings(bindings_path) == [binding]
            with open(focus_path, encoding="utf-8") as file:
                assert file.read().splitlines() == [SECOND_UUID]

            # A second Enter follows the saved mapping and keeps the TUI alive.
            output.clear()
            os.write(master, b"\r")
            wait_for(b"TTYbird stays open")
            assert dashboard.poll() is None, "saved binding focus closed the dashboard"
            with open(focus_path, encoding="utf-8") as file:
                assert file.read().splitlines() == [SECOND_UUID, SECOND_UUID]

            output.clear()
            os.write(master, b"q")
            deadline = time.monotonic() + 10
            while dashboard.poll() is None and time.monotonic() < deadline:
                drain(0.05)
            assert dashboard.poll() == 0, "q did not close the dashboard after focusing"
            drain(0.1)
            terminal_result = json.load(open(result_path, encoding="utf-8"))
            assert terminal_result == {"exit": 0, "restored": True}, terminal_result
            assert b"\x1b[?1049l" in output, "q did not restore the alternate screen"

            print(
                json.dumps(
                    {
                        "provider": "gemini",
                        "synthetic_process_only": True,
                        "cancel_preserved_empty_bindings": True,
                        "stale_picker_rejected_without_binding": True,
                        "stale_picker_focus_calls": 0,
                        "explicit_second_uuid_bound": True,
                        "fake_focus_calls": 2,
                        "dashboard_survived_picker_and_saved_focus": True,
                        "failed_focus_rolled_back_and_kept_dashboard": True,
                        "failed_focus_detail_visible": True,
                        "selected_ghostty_opens_without_p_and_does_not_poll": True,
                        "ghostty_preview_did_not_focus": True,
                        "cancelled_relink_preserved_binding": True,
                        "terminal_restored": True,
                        "exit": 0,
                    }
                )
            )
        finally:
            terminate_owned_group(dashboard)
            for fd in (master, slave):
                if fd is not None:
                    os.close(fd)
            terminate(fixture)


if __name__ == "__main__":
    main()
