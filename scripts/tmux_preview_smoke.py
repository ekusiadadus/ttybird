"""End-to-end TUI preview using a synthetic process on a private tmux server.

python3 scripts/tmux_preview_smoke.py target/debug/ttybird [--gemini]
No real agent receives input. Only summary booleans are printed; pane output is
synthetic and retained only in memory. The private server is always destroyed.
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

binary = os.path.abspath(sys.argv[1])
server = 'ttybird-preview-smoke-' + str(os.getpid())
provider = 'gemini' if '--gemini' in sys.argv else 'codex'


def tmux(*args):
    return subprocess.check_output(['tmux', '-L', server, *args], stderr=subprocess.PIPE)


def terminate_owned_group(process):
    """Stop every process in the fixture-only session and reap its leader."""
    if process is None or process.returncode is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        pass
    # Keep the leader unreaped so its PID cannot be reused as another group's
    # ID before the final fixture-group signal.
    time.sleep(.1)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass
    process.wait(timeout=2)


with tempfile.TemporaryDirectory(prefix='ttybird-synthetic-preview-') as root:
    process = None
    master = slave = None
    try:
        if provider == 'gemini':
            # Actual Node launch shape; synthetic package contents, no model/API.
            entry = root + '/node_modules/@google/gemini-cli/dist/index.js'
            os.makedirs(os.path.dirname(entry))
            with open(entry, 'w') as f:
                f.write('console.log("\\x1b[32mSYNTHETIC_PREVIEW_OK\\x1b[0m");setTimeout(()=>{},90000);')
            launcher = [shutil.which('node') or 'node', entry]
        else:
            # A tiny synthetic executable, not a copied platform-signed binary.
            fake = root + '/codex'
            fixture = '#include <stdio.h>\n#include <unistd.h>\nint main(void){puts("\\033[2J\\033[H\\033[32mSYNTHETIC_PREVIEW_OK\\033[0m");fflush(stdout);sleep(90);return 0;}\n'
            subprocess.run(['cc', '-x', 'c', '-o', fake, '-'], input=fixture.encode(), check=True,
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            launcher = [fake]
        raw = tmux('-f', '/dev/null', 'new-session', '-d', '-x', '80', '-y', '24',
                   '-c', root, '-s', 'fixture', '-P', '-F', '#{socket_path},#{pid},0',
                   *launcher)
        tmux_env = raw.decode().strip()
        env = dict(os.environ, TMUX=tmux_env, TERM='xterm-256color',
                   CODEX_HOME=root+'/empty-codex', CLAUDE_CONFIG_DIR=root+'/empty-claude',
                   CLAUDE_HOME=root+'/empty-claude')
        time.sleep(.5)
        inventory = json.loads(subprocess.check_output([binary,'--local','--config-dir',root,'--json'],env=env))
        fixture_rows = [row for snap in inventory for row in snap['sessions'] if (row.get('cwd') or '').endswith(os.path.basename(root)) and row.get('provider') == provider]
        assert len(fixture_rows) == 1, 'synthetic executable not uniquely recognized by collector'
        assert fixture_rows[0].get('cwd'), 'synthetic process cwd unavailable'
        assert fixture_rows[0]['activity'] == 'unknown', 'process presence must not become working state'
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 140, 0, 0))
        def setup():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        result_path = root + '/terminal-result.json'
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
        process = subprocess.Popen([sys.executable, '-c', wrapper, result_path, binary,
                                    '--local', '--config-dir', root],
                                   stdin=slave, stdout=slave, stderr=slave, env=env,
                                   preexec_fn=setup, close_fds=True)
        output = bytearray()
        def drain(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                if select.select([master], [], [], .04)[0]:
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
                drain(.1)
            if marker not in output:
                import re
                visible = re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', bytes(output))
                raise AssertionError('expected synthetic UI state was not rendered: ' + visible[-1400:].decode('utf-8', 'replace'))
        wait_for(b'TTYbird')
        # Filter before requesting any preview: never capture a real user's pane.
        os.write(master, ('/' + os.path.basename(root)).encode())
        drain(.3)
        os.write(master, b'\r')
        drain(2)
        os.write(master, b'p')
        wait_for(b'SYNTHETIC_PREVIEW_OK')
        output.clear()
        os.write(master, b'p')
        drain(.3)
        os.write(master, b'p')
        wait_for(b'SYNTHETIC_PREVIEW_OK')
        os.write(master, b'q')
        end = time.monotonic() + 5
        while process.poll() is None and time.monotonic() < end:
            drain(.05)
        assert process.poll() == 0, 'dashboard did not exit cleanly'
        result = json.load(open(result_path))
        assert result == {'exit': 0, 'restored': True}, result
        print(json.dumps({'provider': provider, 'synthetic_tmux_preview_rendered': True,
                          'close_and_reopen_rendered': True,
                          'terminal_restored': True, 'exit': 0}))
    finally:
        terminate_owned_group(process)
        for fd in (master, slave):
            if fd is not None:
                os.close(fd)
        subprocess.run(['tmux', '-L', server, 'kill-server'],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
