"""Opt-in real Ghostty focus smoke using only test-owned windows and processes.

Usage:
    python3 scripts/ghostty_focus_smoke.py PATH_TO_TTYBIRD --allow-gui

The test creates two Ghostty windows whose commands are set at creation time.
It never types into a terminal, reads terminal contents, or prints terminal
titles/directories. The only persistent output is a boolean summary at
``/tmp/ttybird-real-ghostty-focus.log``.
"""

import argparse
import datetime
import json
import os
import signal
import subprocess
import sys
import tempfile
import time


SUMMARY_PATH = "/tmp/ttybird-real-ghostty-focus.log"
GHOSTTY_APP = "/Applications/Ghostty.app"
COMMAND_TIMEOUT = 8

FOCUSED_TERMINAL = r'''
tell application "Ghostty"
    if (count of windows) is 0 then return ""
    set selectedWindow to front window
    set selectedTerminal to focused terminal of selected tab of selectedWindow
    if selectedTerminal is missing value then return ""
    return id of selectedTerminal as text
end tell
'''

CREATE_WINDOW = r'''
on run argv
    if (count of argv) is not 2 then error "expected working directory and command"
    set workDir to item 1 of argv
    set commandText to item 2 of argv
    tell application "Ghostty"
        set surfaceConfig to new surface configuration from {initial working directory:workDir, command:commandText}
        set ownedWindow to new window with configuration surfaceConfig
        set ownedTerminal to terminal 1 of ownedWindow
        return (id of ownedWindow as text) & "|" & (id of ownedTerminal as text)
    end tell
end run
'''

FOCUS_TERMINAL = r'''
on run argv
    if (count of argv) is not 1 then error "expected terminal id"
    set terminalID to item 1 of argv
    tell application "Ghostty"
        if not (exists terminal id terminalID) then error "terminal does not exist"
        focus (terminal id terminalID)
    end tell
end run
'''

CLOSE_WINDOW = r'''
on run argv
    if (count of argv) is not 1 then error "expected window id"
    set windowID to item 1 of argv
    tell application "Ghostty"
        if exists window id windowID then close window (window id windowID)
    end tell
end run
'''

WINDOW_EXISTS = r'''
on run argv
    if (count of argv) is not 1 then error "expected window id"
    set windowID to item 1 of argv
    tell application "Ghostty"
        return exists window id windowID
    end tell
end run
'''

TERMINAL_EXISTS = r'''
on run argv
    if (count of argv) is not 1 then error "expected terminal id"
    set terminalID to item 1 of argv
    tell application "Ghostty"
        return exists terminal id terminalID
    end tell
end run
'''

FIXTURE_SOURCE = r'''
#include <signal.h>
#include <unistd.h>

int main(void) {
    for (;;) pause();
    return 0;
}
'''


class SmokeFailure(RuntimeError):
    pass


class AutomationUnavailable(SmokeFailure):
    pass


def summary_template(allow_gui):
    return {
        "allow_gui": bool(allow_gui),
        "automation_access": False,
        "owned_windows_created": False,
        "owned_fixtures_discovered": False,
        "exact_session_bound": False,
        "other_owned_terminal_selected": False,
        "live_focus_command_succeeded": False,
        "requested_owned_terminal_focused": False,
        "killed_fixture_refused_focus": False,
        "failed_focus_preserved_other_owned_terminal": False,
        "fixture_processes_cleaned": False,
        "owned_windows_closed": False,
        "original_terminal_was_present": False,
        "original_terminal_restored": False,
    }


def write_summary(summary):
    with open(SUMMARY_PATH, "w", encoding="utf-8") as output:
        json.dump(summary, output, sort_keys=True)
        output.write("\n")


def applescript(source, *args):
    try:
        result = subprocess.run(
            ["/usr/bin/osascript", "-l", "AppleScript", "-e", source, "--", *args],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=COMMAND_TIMEOUT,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise AutomationUnavailable("Ghostty AppleScript timed out") from error
    if result.returncode != 0:
        if "-1743" in result.stderr or "not authorized" in result.stderr.lower():
            raise AutomationUnavailable("Ghostty AppleScript Automation access was denied")
        raise SmokeFailure("Ghostty AppleScript command failed")
    return result.stdout.strip()


def focused_terminal():
    return applescript(FOCUSED_TERMINAL)


def focus_terminal(terminal_id):
    applescript(FOCUS_TERMINAL, terminal_id)


def create_window(work_dir, command):
    value = applescript(CREATE_WINDOW, work_dir, command)
    fields = value.split("|")
    if len(fields) != 2 or not all(fields):
        raise SmokeFailure("Ghostty did not return owned window identities")
    return fields[0], fields[1]


def close_window(window_id):
    applescript(CLOSE_WINDOW, window_id)


def window_exists(window_id):
    return applescript(WINDOW_EXISTS, window_id).lower() == "true"


def terminal_exists(terminal_id):
    return applescript(TERMINAL_EXISTS, terminal_id).lower() == "true"


def run_cli(binary, env, args, timeout=8):
    return subprocess.run(
        [binary, "--local", "--config-dir", env["TTYBIRD_CONFIG_DIR"], *args],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )


def collect_fixture_rows(binary, env, work_dirs):
    result = run_cli(binary, env, ["--json"])
    if result.returncode != 0:
        return None
    try:
        payload = json.loads(result.stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return None
    matches = {work_dir: [] for work_dir in work_dirs}
    for snapshot in payload:
        for row in snapshot.get("sessions", []):
            cwd = row.get("cwd")
            if row.get("provider") == "codex" and cwd in matches:
                matches[cwd].append(row)
    if any(len(rows) != 1 for rows in matches.values()):
        return None
    rows = {work_dir: found[0] for work_dir, found in matches.items()}
    if any(not row.get("pid") or not row.get("process_started_at") for row in rows.values()):
        return None
    if len({row["pid"] for row in rows.values()}) != len(rows):
        return None
    return rows


def wait_for_fixture_rows(binary, env, work_dirs, seconds=12):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        rows = collect_fixture_rows(binary, env, work_dirs)
        if rows is not None:
            return rows
        time.sleep(0.1)
    raise SmokeFailure("test-owned fixture processes were not uniquely discovered")


def process_start_epoch(pid):
    result = subprocess.run(
        ["/bin/ps", "-o", "lstart=", "-p", str(pid)],
        env=dict(os.environ, LC_ALL="C"),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        timeout=2,
        check=False,
    )
    value = result.stdout.strip()
    if result.returncode != 0 or not value:
        return None
    try:
        parsed = datetime.datetime.strptime(value, "%a %b %d %H:%M:%S %Y")
    except ValueError:
        return None
    return int(parsed.timestamp())


def process_matches(pid, started_at):
    return process_start_epoch(pid) == started_at


def stop_owned_process(pid, started_at):
    if not process_matches(pid, started_at):
        return True
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return True
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        if not process_matches(pid, started_at):
            return True
        time.sleep(0.05)
    if process_matches(pid, started_at):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            return True
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        if not process_matches(pid, started_at):
            return True
        time.sleep(0.05)
    return not process_matches(pid, started_at)


def wait_focused(terminal_id, seconds=5):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if focused_terminal() == terminal_id:
            return True
        time.sleep(0.05)
    return False


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--allow-gui", action="store_true")
    return parser.parse_args()


def main():
    args = parse_args()
    summary = summary_template(args.allow_gui)
    if not args.allow_gui:
        write_summary(summary)
        print("Ghostty GUI smoke skipped: pass --allow-gui to create isolated test windows.", file=sys.stderr)
        return 2
    if sys.platform != "darwin" or not os.path.isdir(GHOSTTY_APP):
        write_summary(summary)
        print("Ghostty GUI smoke unavailable: macOS Ghostty.app is required.", file=sys.stderr)
        return 2

    binary = os.path.abspath(args.binary)
    if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        write_summary(summary)
        print("Ghostty GUI smoke unavailable: ttybird binary is not executable.", file=sys.stderr)
        return 2

    owned_windows = []
    owned_processes = []
    original_terminal = ""
    failed = False
    with tempfile.TemporaryDirectory(prefix="ttybird-ghostty-focus-") as temp_root:
        root = os.path.realpath(temp_root)
        try:
            original_terminal = focused_terminal()
            summary["automation_access"] = True
            summary["original_terminal_was_present"] = bool(original_terminal)

            work_a = os.path.join(root, "fixture-a")
            work_b = os.path.join(root, "fixture-b")
            config_dir = os.path.join(root, "config")
            codex_home = os.path.join(root, "empty-codex")
            claude_home = os.path.join(root, "empty-claude")
            for directory in (work_a, work_b, config_dir, codex_home, claude_home):
                os.makedirs(directory)

            executable = os.path.join(root, "codex")
            compiled = subprocess.run(
                ["/usr/bin/cc", "-x", "c", "-o", executable, "-"],
                input=FIXTURE_SOURCE.encode(),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=15,
                check=False,
            )
            if compiled.returncode != 0:
                raise SmokeFailure("synthetic fixture compilation failed")

            window_a, terminal_a = create_window(work_a, executable)
            owned_windows.append(window_a)
            window_b, terminal_b = create_window(work_b, executable)
            owned_windows.append(window_b)
            summary["owned_windows_created"] = (
                window_a != window_b and terminal_a != terminal_b
            )
            if not summary["owned_windows_created"]:
                raise SmokeFailure("Ghostty returned duplicate owned identities")

            env = dict(
                os.environ,
                CODEX_HOME=codex_home,
                CLAUDE_CONFIG_DIR=claude_home,
                CLAUDE_HOME=claude_home,
                TTYBIRD_CONFIG_DIR=config_dir,
            )
            env.pop("TMUX", None)
            rows = wait_for_fixture_rows(binary, env, [work_a, work_b])
            row_a = rows[work_a]
            row_b = rows[work_b]
            owned_processes.extend(
                [
                    (row_a["pid"], row_a["process_started_at"]),
                    (row_b["pid"], row_b["process_started_at"]),
                ]
            )
            summary["owned_fixtures_discovered"] = True

            bound = run_cli(
                binary,
                env,
                [
                    "bind",
                    row_a["id"],
                    "--pid",
                    str(row_a["pid"]),
                    "--ghostty",
                    terminal_a,
                ],
            )
            if bound.returncode != 0:
                raise SmokeFailure("ttybird refused the exact owned binding")
            with open(os.path.join(config_dir, "bindings.json"), encoding="utf-8") as source:
                bindings = json.load(source)
            summary["exact_session_bound"] = len(bindings) == 1 and bindings[0] == {
                "session_id": row_a["id"],
                "host": row_a["host"],
                "pid": row_a["pid"],
                "process_started_at": row_a["process_started_at"],
                "target": {"kind": "ghostty", "terminal_id": terminal_a},
            }
            if not summary["exact_session_bound"]:
                raise SmokeFailure("saved binding did not match the owned process and terminal")

            focus_terminal(terminal_b)
            summary["other_owned_terminal_selected"] = wait_focused(terminal_b)
            if not summary["other_owned_terminal_selected"]:
                raise SmokeFailure("could not select the second owned terminal")
            focused = run_cli(binary, env, ["focus", row_a["id"]])
            summary["live_focus_command_succeeded"] = focused.returncode == 0
            if not summary["live_focus_command_succeeded"]:
                raise SmokeFailure("ttybird focus failed for the live owned fixture")
            summary["requested_owned_terminal_focused"] = wait_focused(terminal_a)
            if not summary["requested_owned_terminal_focused"]:
                raise SmokeFailure("ttybird focused a terminal other than the requested owned UUID")

            if not stop_owned_process(row_a["pid"], row_a["process_started_at"]):
                raise SmokeFailure("the first owned fixture process did not exit")
            focus_terminal(terminal_b)
            if not wait_focused(terminal_b):
                raise SmokeFailure("could not reselect the second owned terminal")
            stale_focus = run_cli(binary, env, ["focus", row_a["id"]])
            summary["killed_fixture_refused_focus"] = stale_focus.returncode != 0
            summary["failed_focus_preserved_other_owned_terminal"] = wait_focused(terminal_b)
            if not summary["killed_fixture_refused_focus"]:
                raise SmokeFailure("ttybird accepted a killed fixture identity")
            if not summary["failed_focus_preserved_other_owned_terminal"]:
                raise SmokeFailure("failed focus moved away from the other owned terminal")
        except AutomationUnavailable:
            failed = True
            print(
                "Ghostty GUI smoke stopped: AppleScript Automation access was unavailable; no permission or system setting was changed.",
                file=sys.stderr,
            )
        except (SmokeFailure, OSError, subprocess.SubprocessError, json.JSONDecodeError):
            failed = True
            print("Ghostty GUI smoke failed within the isolated fixture.", file=sys.stderr)
        finally:
            process_results = [
                stop_owned_process(pid, started_at)
                for pid, started_at in owned_processes
            ]
            summary["fixture_processes_cleaned"] = all(process_results)

            close_results = []
            for window_id in reversed(owned_windows):
                try:
                    close_window(window_id)
                    close_results.append(not window_exists(window_id))
                except SmokeFailure:
                    close_results.append(False)
            summary["owned_windows_closed"] = (
                len(close_results) == len(owned_windows) and all(close_results)
            )

            try:
                if original_terminal:
                    if terminal_exists(original_terminal):
                        focus_terminal(original_terminal)
                        summary["original_terminal_restored"] = wait_focused(original_terminal)
                else:
                    summary["original_terminal_restored"] = True
            except SmokeFailure:
                summary["original_terminal_restored"] = False

            write_summary(summary)

    success = all(
        value
        for key, value in summary.items()
        if key not in {"original_terminal_was_present"}
    )
    if failed or not success:
        return 1
    print(json.dumps(summary, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
