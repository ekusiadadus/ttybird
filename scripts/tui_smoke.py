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
        wrapper = '''import json,subprocess,sys,termios
before=termios.tcgetattr(0)
p=subprocess.Popen(sys.argv[3:])
open(sys.argv[1],'w').write(str(p.pid))
code=p.wait()
after=termios.tcgetattr(0)
json.dump({'restored':before==after,'exit':code},open(sys.argv[2],'w'))
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
            start = time.monotonic()
            if mode == 'signal':
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
            if process.poll() is None:
                if os.path.exists(pid_path):
                    try:
                        os.kill(int(open(pid_path).read()),signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                process.kill()
                process.wait()
            os.close(master)
            os.close(slave)


print(json.dumps([exercise(mode) for mode in ['keys', 'ctrl-c', 'signal']], indent=2))
