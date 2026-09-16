#!/usr/bin/env python3
"""Real owned PTYs and dashboard; synthetic commands only, no model/API calls."""
import fcntl
import json
import os
import pathlib
import pty
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time

binary = os.path.abspath(sys.argv[1])
with tempfile.TemporaryDirectory(prefix='tb-owned-', dir='/tmp') as root:
    config = pathlib.Path(root) / 'config'
    child = pathlib.Path(root) / 'fixture.py'
    child.write_text('''import os,signal,sys,termios,tty
assert os.isatty(0) and os.isatty(1)
tty.setraw(0)
def size(*args):
 s=os.get_terminal_size(0);os.write(1, f"\\r\\nSIZE={s.columns}x{s.lines}\\r\\n".encode())
signal.signal(signal.SIGWINCH,size)
os.write(1,b"\\x1b[31mMANAGED_READY\\x1b[0m\\r\\nPRIVATE_SCREEN_SENTINEL\\r\\n")
size()
while True:
 data=os.read(0,1024)
 if not data:break
 if b"\\x03" in data:os.write(1,b"\\r\\nCTRL_C_RECEIVED\\r\\n")
 elif data:os.write(1,b"\\r\\nINPUT="+data.hex().encode()+b"\\r\\n")
''')
    base = [binary, '--config-dir', str(config), '--local']
    ids = []
    dashboards = []
    def process_alive(pid):
        try:
            os.kill(pid,0)
            return True
        except ProcessLookupError:
            return False
    def wait_stopped(pids, message):
        deadline=time.monotonic()+8
        while time.monotonic()<deadline and any(process_alive(pid) for pid in pids):
            time.sleep(0.05)
        assert not any(process_alive(pid) for pid in pids), message
    def cli(*args):
        return subprocess.check_output(base + list(args), timeout=12)
    def rpc(sid, payload):
        sockpath = config / 'managed' / (sid + '.sock')
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(3)
            stream.connect(str(sockpath))
            stream.sendall(json.dumps(payload).encode()+b'\n')
            stream.shutdown(socket.SHUT_WR)
            data=b''
            while True:
                chunk=stream.recv(65536)
                if not chunk:break
                data+=chunk
            return json.loads(data)
    class Watch:
        def __init__(self, sid, cols, rows):
            self.stream=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
            self.stream.connect(str(config/'managed'/(sid+'.sock')))
            self.stream.sendall(json.dumps({'Watch':{'cols':cols,'rows':rows}}).encode()+b'\n')
            self.buffer=bytearray()
        def next(self, timeout):
            end=time.monotonic()+timeout
            while b'\n' not in self.buffer:
                remaining=end-time.monotonic()
                if remaining<=0 or not select.select([self.stream],[],[],remaining)[0]:return None
                chunk=self.stream.recv(65536)
                if not chunk:raise AssertionError('screen watch closed before an update')
                self.buffer.extend(chunk)
            line,_,rest=self.buffer.partition(b'\n');self.buffer=bytearray(rest)
            return json.loads(line)
        def close(self):
            self.stream.close()
    def open_dashboard(sid):
        master,slave=pty.openpty()
        fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',32,160,0,0))
        def setup():
            os.setsid();fcntl.ioctl(slave,termios.TIOCSCTTY,0)
        env=dict(os.environ,TERM='xterm-256color',CODEX_HOME=root+'/empty-codex',CLAUDE_CONFIG_DIR=root+'/empty-claude')
        proc=subprocess.Popen(base+['attach',sid],stdin=slave,stdout=slave,stderr=slave,env=env,preexec_fn=setup)
        os.close(slave)
        dashboards.append((proc,master))
        return proc,master,bytearray()
    def wait_text(dashboard, marker):
        proc,master,out=dashboard
        end=time.monotonic()+10
        while marker not in out and time.monotonic()<end:
            if select.select([master],[],[],0.1)[0]:
                try:out.extend(os.read(master,65536))
                except OSError:break
            if proc.poll() is not None:break
        assert marker in out, f'missing {marker!r}; dashboard exit={proc.poll()}; tail={bytes(out[-4096:])!r}'
    def quit_dashboard(dashboard, escape_input=True):
        proc,master,out=dashboard
        os.write(master,(b'\x1d' if escape_input else b'')+b'q')
        end=time.monotonic()+5
        while proc.poll() is None and time.monotonic()<end:
            if select.select([master],[],[],0.1)[0]:
                try:out.extend(os.read(master,65536))
                except OSError:break
        assert proc.wait(timeout=2)==0
    try:
        launched=json.loads(cli('--json','run','--detach','--name','Synthetic owned terminal','--',sys.executable,str(child)))
        sid=launched['id'];ids.append(sid)
        time.sleep(0.2)
        frame=rpc(sid,{'Frame':{'cols':90,'rows':28}})['Frame']['screen']
        assert 'PRIVATE_SCREEN_SENTINEL' in frame['vt']
        assert frame['cols']==90 and frame['rows']==28
        assert (os.stat(config/'managed').st_mode & 0o777)==0o700
        assert (os.stat(config/'managed'/(sid+'.sock')).st_mode & 0o777)==0o600
        watch=Watch(sid,90,28)
        assert 'Frame' in watch.next(3), 'watch did not push an initial frame'
        while watch.next(0.1) is not None:pass
        assert watch.next(0.25) is None, 'idle watch emitted a repeated frame'
        rpc(sid,{'Paste':{'text':'stream\n'}})
        deadline=time.monotonic()+3
        while time.monotonic()<deadline:
            update=watch.next(deadline-time.monotonic())
            if update and '73747265616d0d' in update['Frame']['screen']['vt']:break
        else:raise AssertionError('PTY output did not push a watch frame')
        watch.close()
        watch=Watch(sid,90,28)
        assert '73747265616d0d' in watch.next(3)['Frame']['screen']['vt'], 'reattached watch lost screen'
        watch.close()
        dashboard=open_dashboard(sid)
        wait_text(dashboard,b'MANAGED_READY')
        assert b'Sessions' in dashboard[2]
        os.write(dashboard[1],b'\riq')
        wait_text(dashboard,b'INPUT=')
        assert dashboard[0].poll() is None, 'q in INPUT mode closed dashboard'
        os.write(dashboard[1],b'x')
        wait_text(dashboard,b'INPUT=78')
        assert dashboard[0].poll() is None, 'x in INPUT mode stopped the owned terminal'
        os.write(dashboard[1],b'\x03')
        wait_text(dashboard,b'CTRL_C_RECEIVED')
        assert dashboard[0].poll() is None, 'Ctrl-C bypassed owned PTY'
        os.write(dashboard[1],b'\x1b[200~hello\x1b[201~')
        wait_text(dashboard,b'68656c6c6f')
        quit_dashboard(dashboard)
        listed=json.loads(cli('--json','sessions'))
        assert any(s['id']==sid and not s['ended'] for s in listed), 'detach stopped session'
        frame=rpc(sid,{'Frame':{'cols':90,'rows':28}})['Frame']['screen']
        assert 'CTRL_C_RECEIVED' in frame['vt'], 'screen state lost on detach'
        unrelated=json.loads(cli('--json','run','--detach','--name','Unrelated owned terminal','--',sys.executable,str(child)))
        unrelated_id=unrelated['id'];ids.append(unrelated_id)
        unrelated_pids=(unrelated['pid'],unrelated['daemon_pid'])
        assert all(process_alive(pid) for pid in unrelated_pids), 'unrelated fixture did not stay live'
        dashboard=open_dashboard(sid)
        wait_text(dashboard,b'CTRL_C_RECEIVED')
        selected_pids=(launched['pid'],launched['daemon_pid'])
        dashboard[2].clear()
        os.write(dashboard[1],b'\r')
        time.sleep(0.25)
        os.write(dashboard[1],b'\x1d')
        time.sleep(0.25)
        assert dashboard[0].poll() is None, 'Ctrl+] closed dashboard before in-TUI stop'
        dashboard[2].clear()
        os.write(dashboard[1],b'x')
        wait_text(dashboard,b'Stopping owned terminal\xe2\x80\xa6')
        # Ratatui diff rendering can split the changed "Stopped" prefix with
        # cursor movements; this completion-only suffix remains contiguous.
        wait_text(dashboard,b'owned terminal. TTYbird stays open.')
        wait_stopped(selected_pids,'in-TUI stop left selected child or helper alive')
        assert dashboard[0].poll() is None, 'in-TUI stop closed dashboard'
        assert all(process_alive(pid) for pid in unrelated_pids), 'in-TUI stop killed unrelated fixture'
        other_frame=rpc(unrelated_id,{'Frame':{'cols':90,'rows':28}})['Frame']['screen']
        assert 'PRIVATE_SCREEN_SENTINEL' in other_frame['vt'], 'unrelated fixture stopped responding'
        quit_dashboard(dashboard,escape_input=False)
        assert all(process_alive(pid) for pid in unrelated_pids), 'dashboard quit killed unrelated fixture'
        for path in config.rglob('*'):
            if path.is_file():
                assert b'PRIVATE_SCREEN_SENTINEL' not in path.read_bytes(), 'screen was persisted'
        assert not (config/'managed'/(sid+'.sock')).exists(), 'stop left live socket'
        ended=json.loads(cli('--json','sessions'))
        assert next(s for s in ended if s['id']==sid)['ended']
        bad=subprocess.run(base+['run','--detach','--','/no/such/ttybird-test-command'],capture_output=True,timeout=10)
        assert bad.returncode!=0, 'invalid command silently succeeded'
        fast=json.loads(cli('--json','run','--detach','--',sys.executable,'-c','pass'))
        ids.append(fast['id'])
        deadline=time.monotonic()+3
        while time.monotonic()<deadline:
            history=json.loads(cli('--json','sessions'))
            if any(s['id']==fast['id'] and s['ended'] and s['exit_code']==0 for s in history):break
            time.sleep(0.05)
        else:raise AssertionError('short command did not reach ended status')
        print(json.dumps({'real_owned_pty':True,'event_driven_screen':True,'no_idle_frames':True,'libghostty_screen':True,'input_and_paste':True,'ctrl_c_routed':True,'detach_reconnect':True,'in_tui_stop':True,'unrelated_owned_survived':True,'resize':True,'no_screen_persistence':True,'stop_cleanup':True,'no_real_agents':True}))
    finally:
        for proc,master in dashboards:
            os.close(master)
            if proc.poll() is None:
                proc.terminate()
                try:proc.wait(timeout=3)
                except subprocess.TimeoutExpired:proc.kill();proc.wait()
        # Recover IDs even if the launch client timed out before returning one.
        for path in (config/'managed').glob('*.json'):
            if path.stem not in ids:ids.append(path.stem)
        for sid in ids:
            try:rpc(sid,'Stop')
            except (OSError,ValueError):pass
