"""Exercise the actual dashboard in an isolated PTY; never sends input to agents.

Usage: python3 scripts/tui_smoke.py PATH_TO_TTYBIRD
Uses process-only collection, temporary config, and no SSH hosts.
"""
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


def terminate_owned_group(process):
    """Stop every process in a fixture-only session and reap its leader."""
    if process is None or process.returncode is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        pass
    # Do not reap the leader before the final process-group signal. While it is
    # a zombie its PID cannot be reused for an unrelated process group.
    time.sleep(.1)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass
    process.wait(timeout=2)


def exercise(mode):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 120, 0, 0))
    before = termios.tcgetattr(slave)
    output = bytearray()
    with tempfile.TemporaryDirectory(prefix='ttybird-tui-test-') as root:
        env = dict(os.environ, TERM='xterm-256color', CODEX_HOME=root+'/codex',
                   CLAUDE_CONFIG_DIR=root+'/claude', CLAUDE_HOME=root+'/claude')

        def setup():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        # Keep the PTY session leader alive until after attributes are checked.
        # macOS revokes its slave after the controlling session leader exits.
        pid_path, result_path = root+'/child.pid', root+'/result.json'
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
    p=subprocess.Popen(sys.argv[3:])
    open(sys.argv[1],'w').write(str(p.pid))
    code=p.wait()
finally:
    if p is not None and p.poll() is None:
        p.kill();p.wait()
after=termios.tcgetattr(0)
restored=comparable(before)==comparable(after) and flags&mutable==fcntl.fcntl(0,fcntl.F_GETFL)&mutable
json.dump({'restored':restored,'exit':code},open(sys.argv[2],'w'))
sys.exit(code)
'''
        process = subprocess.Popen([sys.executable,'-c',wrapper,pid_path,result_path,sys.argv[1], '--local', '--config-dir', root],
                                   stdin=slave, stdout=slave, stderr=slave, env=env,
                                   preexec_fn=setup, close_fds=True)

        def drain(seconds):
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                if select.select([master], [], [], .04)[0]:
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        break
                    if not data:
                        break
                    output.extend(data)

        try:
            for _ in range(60):
                drain(.1)
                if b'TTYbird' in output:
                    break
            assert b'TTYbird' in output, 'dashboard did not open'
            if mode == 'keys':
                drain(.8)
                # Exercise tree keys on the dashboard, never on agent terminals.
                os.write(master, b' \x1b[D\x1b[C')
                drain(.15)
                os.write(master, b'd')
                drain(.2)
                assert b'Esc closes' in output, 'details overlay did not open'
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
                os.kill(process.pid, signal.SIGWINCH)
                drain(.2)
                os.write(master, b'\x1b')
                drain(.15)
                os.write(master, b'/zz-ttybird-no-match')
                drain(.2)
                assert b'Search' in output, 'search did not open'
                os.write(master, b'\x1b')
                drain(.15)
                os.write(master, b'/zz-ttybird-no-match\rp')
                drain(.25)
                assert b'Terminal preview' in output, 'preview did not open'
                os.write(master, b'\x1b[6~\x1b[5~')
                drain(.1)
                os.write(master, b'p')
                os.write(master, b'br')
                drain(.15)
            if mode == 'partial-signal':
                # An incomplete UTF-8 sequence must not leave Crossterm in a
                # blocking follow-up read that ignores the termination flag.
                os.write(master, b'\xc3')
                time.sleep(.04)
            start = time.monotonic()
            if mode in ('signal', 'partial-signal'):
                os.kill(int(open(pid_path).read()), signal.SIGTERM)
            else:
                os.write(master, b'\x03' if mode == 'ctrl-c' else b'q')
            while process.poll() is None and time.monotonic()-start < 5:
                drain(.05)
            assert process.poll() == 0, f'{mode} did not exit cleanly: {process.poll()}'
            drain(.1)
            assert json.load(open(result_path))['restored'], f'{mode} did not restore terminal attributes'
            assert b'\x1b[?1049l' in output, 'alternate screen was not restored'
            return {'mode': mode, 'exit': process.returncode, 'terminal_restored': True,
                    'quit_seconds': round(time.monotonic()-start, 3)}
        finally:
            terminate_owned_group(process)
            os.close(master)
            os.close(slave)


def exercise_hangup(ignore_sighup, close_delay):
    """Closing the private PTY must not strand a CPU-spinning dashboard."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 120, 0, 0))
    output = bytearray()
    process = None
    with tempfile.TemporaryDirectory(prefix='ttybird-tui-hangup-') as root:
        env = dict(os.environ, TERM='xterm-256color', CODEX_HOME=root+'/codex',
                   CLAUDE_CONFIG_DIR=root+'/claude', CLAUDE_HOME=root+'/claude')

        def setup():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            if ignore_sighup:
                signal.signal(signal.SIGHUP, signal.SIG_IGN)

        try:
            process = subprocess.Popen(
                [sys.argv[1], '--local', '--config-dir', root],
                stdin=slave, stdout=slave, stderr=slave, env=env,
                preexec_fn=setup, close_fds=True)
            # The dashboard now owns the only slave descriptors. Closing the
            # master below therefore produces a real PTY hangup/EOF.
            os.close(slave)
            slave = None
            deadline = time.monotonic() + 6
            while b'TTYbird' not in output and time.monotonic() < deadline:
                if process.poll() is not None:
                    break
                if select.select([master], [], [], .04)[0]:
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        break
                    if not data:
                        break
                    output.extend(data)
            assert b'TTYbird' in output, 'dashboard did not open for hangup test'

            # Vary closure across successive timed reads, including immediately
            # after a draw and while the dashboard should be blocked in poll.
            time.sleep(close_delay)
            start = time.monotonic()
            os.close(master)
            master = None
            try:
                code = process.wait(timeout=2)
            except subprocess.TimeoutExpired as error:
                raise AssertionError('dashboard survived PTY hangup') from error
            assert code == 0, f'dashboard did not handle PTY hangup cleanly: exit={code}, ignore_sighup={ignore_sighup}, delay_ms={round(close_delay*1000)}'
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                pass
            else:
                raise AssertionError('dashboard left a process in its fixture group')
            return {
                'mode': 'hangup-sighup-ignored' if ignore_sighup else 'hangup',
                'exit': code,
                'close_delay_ms': round(close_delay * 1000),
                'exit_seconds': round(time.monotonic()-start, 3),
            }
        finally:
            terminate_owned_group(process)
            for fd in (master, slave):
                if fd is not None:
                    os.close(fd)


results = [exercise(mode) for mode in ['keys', 'ctrl-c', 'signal', 'partial-signal']]
results.extend(
    exercise_hangup(ignore, delay)
    for ignore in (False, True)
    for delay in (0, .04, .12)
)
print(json.dumps(results, indent=2))
