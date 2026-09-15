"""Exercise live, historical, and zombie identity handling with a synthetic process.

Usage: python3 scripts/liveness_smoke.py PATH_TO_TTYBIRD

The fixture is a temporary executable named ``codex``. It owns a private PTY
and a synthetic Codex JSONL file; no existing terminal receives input. Output
contains summary booleans only, and every child is reaped during cleanup.
"""

import datetime
import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


SESSION_ID = "00000000-0000-4000-8000-000000000701"
HANGUP_SESSION_ID = "00000000-0000-4000-8000-000000000702"
FAKE_GHOSTTY_UUID = "00000000-0000-4000-8000-000000000799"
POLL_SECONDS = 4.0


FIXTURE_SOURCE = r'''
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 4) return 64;
    signal(SIGHUP, SIG_IGN);
    signal(SIGPIPE, SIG_IGN);
    setvbuf(stdout, NULL, _IOLBF, 0);
    int log_fd = open(argv[1], O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (log_fd < 0) return 65;
    dprintf(log_fd,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"%s\",\"cwd\":\"%s\"}}\n",
        argv[2], argv[3]);
    fsync(log_fd);
    printf("READY %ld\n", (long)getpid());

    char command[128];
    for (;;) {
        if (fgets(command, sizeof(command), stdin) == NULL) {
            clearerr(stdin);
            usleep(20000);
            continue;
        }
        if (strcmp(command, "close_log\n") == 0 || strcmp(command, "close_log\r\n") == 0) {
            if (log_fd >= 0) close(log_fd);
            log_fd = -1;
            puts("CLOSED");
        } else if (strcmp(command, "exit\n") == 0 || strcmp(command, "exit\r\n") == 0) {
            if (log_fd >= 0) close(log_fd);
            puts("EXITING");
            return 0;
        }
    }
}
'''


def normalized_tty(value):
    value = value.strip()
    if value.startswith("/dev/"):
        value = value[5:]
    return None if value in ("", "?", "??", "-") else value


def ps_field(pid, field):
    result = subprocess.run(
        ["ps", "-o", field + "=", "-p", str(pid)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env=dict(os.environ, LC_ALL="C"),
        timeout=2,
    )
    return result.stdout.strip() if result.returncode == 0 else ""


def ps_start_epoch(pid):
    value = ps_field(pid, "lstart")
    parsed = datetime.datetime.strptime(value, "%a %b %d %H:%M:%S %Y")
    return int(parsed.timestamp())


class Fixture:
    def __init__(self, executable, log_path, session_id, cwd):
        self.master, slave = pty.openpty()
        self.tty = os.ttyname(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))

        def setup():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(
            [executable, log_path, session_id, cwd],
            cwd=cwd,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            preexec_fn=setup,
            close_fds=True,
        )
        os.close(slave)
        self.output = bytearray()
        self.wait_for(b"READY", 2.0)

    @property
    def pid(self):
        return self.process.pid

    def drain(self, seconds):
        end = time.monotonic() + seconds
        while self.master is not None and time.monotonic() < end:
            if not select.select([self.master], [], [], 0.03)[0]:
                continue
            try:
                data = os.read(self.master, 4096)
            except OSError:
                break
            if not data:
                break
            self.output.extend(data)

    def wait_for(self, marker, seconds=2.0):
        deadline = time.monotonic() + seconds
        while marker not in self.output and time.monotonic() < deadline:
            self.drain(0.05)
        assert marker in self.output, "synthetic fixture did not acknowledge command"

    def command(self, command, marker):
        assert self.master is not None
        self.output.clear()
        os.write(self.master, command.encode("ascii") + b"\n")
        self.wait_for(marker)

    def close_master(self):
        if self.master is not None:
            os.close(self.master)
            self.master = None

    def reap(self):
        self.process.wait(timeout=3)

    def cleanup(self):
        if self.process.returncode is None:
            try:
                os.kill(self.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                try:
                    os.kill(self.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.process.wait(timeout=2)
        self.close_master()


def flatten(payload):
    return [row for snapshot in payload for row in snapshot["sessions"]]


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: liveness_smoke.py PATH_TO_TTYBIRD")
    binary = os.path.abspath(sys.argv[1])
    if not os.path.isfile(binary):
        raise RuntimeError("ttybird binary does not exist")

    with tempfile.TemporaryDirectory(prefix="ttybird-liveness-") as temp_root:
        root = os.path.realpath(temp_root)
        codex_home = os.path.join(root, "codex-home")
        claude_home = os.path.join(root, "claude-home")
        config_dir = os.path.join(root, "config")
        sessions = os.path.join(codex_home, "sessions", "synthetic")
        os.makedirs(sessions)
        os.makedirs(claude_home)
        os.makedirs(config_dir)

        executable = os.path.join(root, "codex")
        subprocess.run(
            ["cc", "-x", "c", "-o", executable, "-"],
            input=FIXTURE_SOURCE.encode(),
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        env = dict(
            os.environ,
            CODEX_HOME=codex_home,
            CLAUDE_CONFIG_DIR=claude_home,
            CLAUDE_HOME=claude_home,
            TTYBIRD_CONFIG_DIR=config_dir,
        )

        def snapshot():
            payload = subprocess.check_output(
                [binary, "--local", "--config-dir", config_dir, "--json"],
                env=env,
                timeout=3,
            )
            return json.loads(payload)

        def poll(predicate, description):
            deadline = time.monotonic() + POLL_SECONDS
            last = None
            while time.monotonic() < deadline:
                last = snapshot()
                if predicate(flatten(last)):
                    return last
                time.sleep(0.08)
            raise AssertionError(description)

        log_path = os.path.join(sessions, "live.jsonl")
        fixture = Fixture(executable, log_path, SESSION_ID, root)
        hangup_fixture = None
        try:
            alive = poll(
                lambda rows: any(
                    row["id"] == SESSION_ID and row.get("pid") == fixture.pid
                    for row in rows
                ),
                "writable log was not associated with its live process",
            )
            row = next(row for row in flatten(alive) if row["id"] == SESSION_ID)
            assert row["process_started_at"] == ps_start_epoch(fixture.pid)
            assert normalized_tty(row["tty"]) == normalized_tty(ps_field(fixture.pid, "tty"))
            assert row["target"] is None

            binding = {
                "session_id": SESSION_ID,
                "host": row["host"],
                "pid": fixture.pid,
                "process_started_at": row["process_started_at"],
                "target": {"kind": "ghostty", "terminal_id": FAKE_GHOSTTY_UUID},
            }
            with open(os.path.join(config_dir, "bindings.json"), "w", encoding="utf-8") as file:
                json.dump([binding], file)
            bound = poll(
                lambda rows: any(
                    candidate["id"] == SESSION_ID and candidate.get("target") == binding["target"]
                    for candidate in rows
                ),
                "exact live binding was not applied",
            )
            assert next(row for row in flatten(bound) if row["id"] == SESSION_ID)["pid"] == fixture.pid

            fixture.command("close_log", b"CLOSED")
            detached = poll(
                lambda rows: any(
                    candidate["id"] == SESSION_ID
                    and candidate.get("pid") is None
                    and candidate.get("process_started_at") is None
                    and candidate.get("tty") is None
                    and candidate.get("target") is None
                    for candidate in rows
                )
                and any(
                    candidate["id"].startswith("codex-pid-")
                    and candidate.get("pid") == fixture.pid
                    for candidate in rows
                ),
                "closed log retained live identity or lost the process-only row",
            )
            detached_rows = flatten(detached)
            assert sum(row["id"] == SESSION_ID for row in detached_rows) == 1

            fixture.command("exit", b"EXITING")
            zombie = poll(
                lambda rows: ps_field(fixture.pid, "stat").startswith("Z")
                and all(candidate.get("pid") != fixture.pid for candidate in rows),
                "unreaped zombie was reported as a live process",
            )
            zombie_history = next(row for row in flatten(zombie) if row["id"] == SESSION_ID)
            assert zombie_history["pid"] is None
            assert zombie_history["tty"] is None
            fixture.reap()

            reaped = poll(
                lambda rows: all(candidate.get("pid") != fixture.pid for candidate in rows)
                and any(
                    candidate["id"] == SESSION_ID
                    and candidate.get("tty") is None
                    and candidate.get("target") is None
                    for candidate in rows
                ),
                "reaped process identity remained attached",
            )
            assert ps_field(fixture.pid, "stat") == ""

            hangup_log = os.path.join(sessions, "hangup.jsonl")
            hangup_fixture = Fixture(executable, hangup_log, HANGUP_SESSION_ID, root)
            poll(
                lambda rows: any(
                    row["id"] == HANGUP_SESSION_ID and row.get("pid") == hangup_fixture.pid
                    for row in rows
                ),
                "PTY hangup fixture was not discovered",
            )
            hangup_fixture.close_master()
            hangup_verified = False
            hangup_row = None
            current_ps_tty = None
            deadline = time.monotonic() + POLL_SECONDS
            while time.monotonic() < deadline:
                current_ps_tty = normalized_tty(ps_field(hangup_fixture.pid, "tty"))
                current = snapshot()
                hangup_row = next(
                    (row for row in flatten(current) if row["id"] == HANGUP_SESSION_ID), None
                )
                if current_ps_tty is None and hangup_row is not None:
                    assert hangup_row.get("pid") == hangup_fixture.pid
                    assert hangup_row.get("tty") is None
                    hangup_verified = True
                    break
                time.sleep(0.08)
            hangup_status = ps_field(hangup_fixture.pid, "stat")
            assert hangup_status and not hangup_status.startswith("Z")
            assert hangup_row is not None and hangup_row.get("pid") == hangup_fixture.pid
            assert normalized_tty(hangup_row.get("tty") or "") == current_ps_tty

            print(
                json.dumps(
                    {
                        "synthetic_fixture_only": True,
                        "live_pid_start_tty_matched_ps": True,
                        "closed_log_became_history_without_binding": True,
                        "process_only_row_remained_live": True,
                        "zombie_excluded": True,
                        "reaped_process_excluded": True,
                        "pty_master_close_process_retained": True,
                        "post_hangup_tty_matched_ps": True,
                        "pty_hangup_tty_cleared": hangup_verified,
                        "exit": 0,
                    },
                    sort_keys=True,
                )
            )
        finally:
            if hangup_fixture is not None:
                hangup_fixture.cleanup()
            fixture.cleanup()


if __name__ == "__main__":
    main()
