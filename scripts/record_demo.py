"""Record the real TUI and Enter navigation on a private tmux server.

uv run --with pillow scripts/record_demo.py
Only synthetic agent processes/messages are captured. No model/API is called.
Never sends input to existing sessions. Every exported cell is privacy-checked.
"""
import datetime
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / 'target/release/ttybird'
RENDERER = ROOT / 'target/release/examples/capture_frame'
SERVER = 'ttybird-record-' + str(os.getpid())
FORBIDDEN = ('toioswarm', 'isucon', 'rovnou', 'livepass', 'ekusiadadus', '/users/', '/home/')
FONT = ImageFont.truetype('/System/Library/Fonts/Menlo.ttc', 17)
HEADLINE = ImageFont.truetype('/System/Library/Fonts/Menlo.ttc', 26)
SMALL = ImageFont.truetype('/System/Library/Fonts/Menlo.ttc', 14)
frames = []
metadata = []
client = None
master = slave = None

def tmux(*args):
    return subprocess.check_output(['tmux', '-L', SERVER, *args], stderr=subprocess.PIPE)

def settle(seconds=.3):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if master is not None and select.select([master], [], [], .02)[0]:
            try:
                if not os.read(master, 65536): break
            except OSError: break

def keys(pane, *values):
    tmux('send-keys', '-t', pane, *values)
    settle()

def wait_text(pane, marker, timeout=12):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        settle(.1)
        text = tmux('capture-pane', '-p', '-t', pane).decode()
        if marker in text: return
    if not any(word in text.lower() for word in FORBIDDEN):
        Path('/tmp/ttybird-demo-failure.txt').write_text(text)
    raise AssertionError('Synthetic UI state not reached: ' + marker)

def pane_text(pane):
    return tmux('capture-pane', '-p', '-t', pane).decode()

def assert_synthetic_inventory(pane):
    text = pane_text(pane)
    assert '3 shown / 3 entries' in text, 'dashboard contains non-fixture session metadata'
    assert '1 live PIDs / 1 host TTYs' in text, 'dashboard process inventory is not fixture-only'
    for title in ('Build TTYbird', 'Polish session titles', 'Verify terminal cleanup'):
        assert title in text, 'expected synthetic session is missing: ' + title

def capture(pane, caption, explanation, key="", duration=1200, png=None):
    cols, rows = map(int, tmux('display-message', '-p', '-t', pane, '#{pane_width} #{pane_height}').split())
    raw = tmux('capture-pane', '-p', '-e', '-t', pane)
    data = json.loads(subprocess.check_output([str(RENDERER), str(cols), str(rows)], input=raw))
    text = '\n'.join(''.join(c['text'] for c in data['cells'] if c['y'] == y) for y in range(rows)).lower()
    assert not any(word in text for word in FORBIDDEN), 'Private text detected; refusing to export frame'
    with tempfile.TemporaryDirectory(prefix='ttybird-frame-') as directory:
        cells, image = Path(directory)/'cells.json', Path(directory)/'screen.png'
        cells.write_text(json.dumps(data))
        subprocess.run([sys.executable, str(ROOT/'scripts/render_preview.py'), str(cells), str(image)], check=True)
        with Image.open(image) as opened: ui = opened.convert('RGB')
    frame = Image.new('RGB', (ui.width, ui.height+124), '#101820')
    frame.paste(ui, (0,124))
    draw = ImageDraw.Draw(frame)
    draw.text((18,10), 'TTYbird  /  real app interaction  /  synthetic agents and messages', font=SMALL, fill='#7e919d')
    draw.text((18,39), caption, font=HEADLINE, fill='#39c5bb')
    draw.text((18,81), explanation, font=FONT, fill='#dce7ed')
    if key:
        width=draw.textbbox((0,0),key,font=HEADLINE)[2]+32
        draw.rounded_rectangle((ui.width-width-20,32,ui.width-20,73),radius=7,fill='#243a46',outline='#39c5bb')
        draw.text((ui.width-width-4,36),key,font=HEADLINE,fill='#ffffff')
    frames.append(frame)
    metadata.append({'caption':caption, 'explanation':explanation, 'key':key, 'duration_ms':duration, 'cols':cols, 'rows':rows, 'privacy_check':'passed'})
    if png: frame.save(ROOT/'docs'/png)

with tempfile.TemporaryDirectory(prefix='ttybird-demo-', dir='/tmp') as directory:
    root = Path(directory)
    workspace = root/'ttybird'
    workspace.mkdir()
    codex = root/'codex-home'
    logs = codex/'sessions'
    logs.mkdir(parents=True)
    stamp = datetime.datetime.now(datetime.timezone.utc).isoformat()
    logpaths = []
    titles = []
    for name, title, tokens in [('parent','Build TTYbird',14200), ('child-ui','Polish session titles',3800), ('child-tests','Verify terminal cleanup',2400)]:
        sid = 'recording-' + name
        payload = {'id':sid, 'cwd':str(workspace), 'title':title, 'source':'cli'}
        if name != 'parent': payload['parent_thread_id']='recording-parent'
        records = [
            {'timestamp':stamp,'type':'session_meta','payload':payload},
            {'timestamp':stamp,'type':'turn_context','payload':{'model':'gpt-6-astra' if name=='parent' else 'gpt-5.6-sol'}},
            {'timestamp':stamp,'type':'response_item','payload':{'type':'message','role':'user','content':[{'type':'input_text','text':'Synthetic demo request: make the session list easier to read.'}]}},
            {'timestamp':stamp,'type':'response_item','payload':{'type':'message','role':'assistant','content':[{'type':'output_text','text':'Synthetic demo reply: show titles and token usage, then verify terminal cleanup.'}]}},
            {'timestamp':stamp,'type':'event_msg','payload':{'type':'token_count','info':{'total_token_usage':{'input_tokens':tokens-1000,'cached_input_tokens':1000,'output_tokens':1000,'reasoning_output_tokens':250,'total_tokens':tokens}}}},
            {'timestamp':stamp,'type':'event_msg','payload':{'type':'task_started'}},
        ]
        path=logs/(sid+'.jsonl');path.write_text(''.join(json.dumps(r)+'\n' for r in records));logpaths.append(path)
        titles.append({'id':sid,'thread_name':title})
    (codex/'session_index.jsonl').write_text(''.join(json.dumps(v)+'\n' for v in titles))
    fake=root/'codex'
    source=r'''#include <fcntl.h>
#include <stdio.h>
#include <signal.h>
#include <unistd.h>
static void updated(int sig){(void)sig;const char text[]="\r\nPreview updated from the original pane.\r\n";write(1,text,sizeof(text)-1);}
int main(int argc,char **argv){signal(SIGUSR1,updated);for(int i=1;i<argc;i++) if(open(argv[i],O_WRONLY|O_APPEND)<0)return 2;
printf("\033[2J\033[H\033[1;36mTTYbird demo workspace\033[0m\n\nSynthetic Astra parent + Sol children\n\n\033[32mPASS\033[0m  terminal hangup exits cleanly\n\033[32mPASS\033[0m  session titles and usage are visible\n\033[32mPASS\033[0m  private conversations are opt-in\n\nEnter returned to this exact tmux pane.\nNo real agent or API is used in this recording.\n");fflush(stdout);for(;;)pause();}'''
    subprocess.run(['cc','-x','c','-o',str(fake),'-'],input=source.encode(),check=True)
    try:
        agent=tmux('-f','/dev/null','new-session','-d','-x','160','-y','28','-s','recording','-n','agent','-c',str(workspace),'-P','-F','#{pane_id}',str(fake),*[str(p) for p in logpaths]).decode().strip()
        tmux('set-option','-g','status','off')
        wait_text(agent,'TTYbird demo workspace')

        # sysinfo reads macOS's process table directly, so PATH wrappers alone
        # cannot isolate the demo from the user's real coding agents. Interpose
        # only proc_listallpids in the fixture dashboard: sysinfo still reads
        # the real PID/start time/cwd/argv for the owned process, but it cannot
        # enumerate any unrelated PID. The dashboard itself is included so it
        # can establish its UID; it does not classify as a coding provider.
        interposer = root/'fixture-processes.dylib'
        interposer_source = r'''
#include <libproc.h>
#include <sys/sysctl.h>
#include <string.h>
#include <errno.h>
#include <stdlib.h>
#include <unistd.h>

static int fixture_proc_listallpids(void *buffer, int buffersize) {
    if (buffer == NULL || buffersize <= 0) return 16;
    pid_t values[2];
    int count = 0;
    values[count++] = getpid();
    const char *raw = getenv("TTYBIRD_DEMO_PID");
    if (raw != NULL) {
        long value = strtol(raw, NULL, 10);
        if (value > 0 && value != values[0]) values[count++] = (pid_t)value;
    }
    int capacity = buffersize / (int)sizeof(pid_t);
    if (count > capacity) count = capacity;
    for (int i = 0; i < count; i++) ((pid_t *)buffer)[i] = values[i];
    return count;
}

#define INTERPOSE(_replacement, _replacee) \
    __attribute__((used)) static struct { const void *replacement; const void *replacee; } \
    interpose_##_replacee __attribute__((section("__DATA,__interpose"))) = \
        { (const void *)(unsigned long)&_replacement, (const void *)(unsigned long)&_replacee }
INTERPOSE(fixture_proc_listallpids, proc_listallpids);
// Isolate the displayed hostname as well; other sysctl requests use the OS.
static int fixture_sysctl(int *name, u_int namelen, void *oldp, size_t *oldlenp, void *newp, size_t newlen) {
    if(namelen==2 && name[0]==CTL_KERN && name[1]==KERN_HOSTNAME && newp==NULL && oldlenp!=NULL){
        const char host[]="demo-mac";
        size_t available=*oldlenp; *oldlenp=sizeof(host);
        if(oldp!=NULL){if(available<sizeof(host)){errno=ENOMEM;return -1;}memcpy(oldp,host,sizeof(host));}
        return 0;
    }
    return sysctl(name,namelen,oldp,oldlenp,newp,newlen);
}
INTERPOSE(fixture_sysctl, sysctl);
'''
        subprocess.run(
            ['cc','-dynamiclib','-x','c','-o',str(interposer),'-'],
            input=interposer_source.encode(), check=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )

        fake_bin = root/'fixture-bin'
        fake_bin.mkdir()
        real_ps = shutil.which('ps')
        real_lsof = shutil.which('lsof')
        assert real_ps and real_lsof, 'ps and lsof are required for the demo'
        tool_wrapper = r'''#!/bin/sh
allowed="${TTYBIRD_DEMO_PID:?}"
requested=""
previous=""
for value in "$@"; do
    if [ "$previous" = "-p" ]; then requested="$value"; break; fi
    previous="$value"
done
[ "$requested" = "$allowed" ] || exit 64
unset DYLD_INSERT_LIBRARIES
exec __REAL_TOOL__ "$@"
'''
        for name, target in (('ps',real_ps),('lsof',real_lsof)):
            path=fake_bin/name
            path.write_text(tool_wrapper.replace('__REAL_TOOL__',shlex.quote(target)))
            path.chmod(0o755)

        collector_env = dict(
            os.environ,
            CODEX_HOME=str(codex),
            CLAUDE_CONFIG_DIR=str(root/'empty-claude'),
            CLAUDE_HOME=str(root/'empty-claude'),
            PATH=str(fake_bin)+os.pathsep+os.environ['PATH'],
            DYLD_INSERT_LIBRARIES=str(interposer),
            TTYBIRD_DEMO_PID=str(int(tmux('display-message','-p','-t',agent,'#{pane_pid}'))),
        )
        collector_env.pop('TMUX',None)
        inventory=json.loads(subprocess.check_output(
            [str(BINARY),'--local','--config-dir',str(root/'config'),'--json'],
            env=collector_env, timeout=10,
        ))
        assert all(snapshot['host']=='demo-mac' for snapshot in inventory), 'fixture hostname isolation failed'
        sessions=[row for snapshot in inventory for row in snapshot['sessions']]
        fixture_pid=int(collector_env['TTYBIRD_DEMO_PID'])
        assert len(sessions)==3, 'collector inventory is not limited to three synthetic sessions'
        assert all(row.get('pid')==fixture_pid for row in sessions), 'synthetic sessions lost exact PID identity'
        assert all(row.get('cwd')==str(workspace) for row in sessions), 'synthetic sessions lost exact cwd identity'

        command=shlex.join([
            'env','-u','NO_COLOR',
            'CODEX_HOME='+str(codex),
            'CLAUDE_CONFIG_DIR='+str(root/'empty-claude'),
            'CLAUDE_HOME='+str(root/'empty-claude'),
            'PATH='+collector_env['PATH'],
            'DYLD_INSERT_LIBRARIES='+str(interposer),
            'TTYBIRD_DEMO_PID='+str(fixture_pid),
            str(BINARY),'--local','--config-dir',str(root/'config'),
        ])
        dashboard=tmux('new-window','-t','recording','-n','dashboard','-c',str(workspace),'-P','-F','#{pane_id}',command).decode().strip()
        master,slave=pty.openpty()
        fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',28,160,0,0))
        def setup():
            os.setsid();fcntl.ioctl(slave,termios.TIOCSCTTY,0)
        client=subprocess.Popen(['tmux','-L',SERVER,'attach-session','-t','recording'],stdin=slave,stdout=slave,stderr=slave,preexec_fn=setup,env=dict(os.environ,TERM='xterm-256color'))
        os.close(slave);slave=None
        wait_text(dashboard,'Build TTYbird')
        assert_synthetic_inventory(dashboard)
        capture(dashboard,'Find your coding agents',
                'Session titles, models and token usage in one place.', duration=1900, png='tui-preview.png')
        keys(dashboard,'Down');wait_text(dashboard,'Tokens  2400')
        capture(dashboard,'See what each child is doing',
                'Astra coordinates; each Sol child keeps its own session.', key='Down', duration=1500)
        keys(dashboard,'Down');wait_text(dashboard,'Tokens  3800')
        capture(dashboard,'Check recorded usage',
                'Per-session tokens; cached input is included, not added twice.', key='Down', duration=1700)
        keys(dashboard,'Left');wait_text(dashboard,'Tokens  14200')
        capture(dashboard,'Return to the parent', 'Left moves from a child to its parent.', key='Left', duration=700)
        keys(dashboard,'Space');wait_text(dashboard,'1 shown / 3 entries')
        assert 'Polish session titles' not in pane_text(dashboard), 'fold did not hide children'
        capture(dashboard,'Less clutter in one keystroke', 'Fold the branch without stopping any agent.', key='Space', duration=1100)
        keys(dashboard,'Right');wait_text(dashboard,'3 shown / 3 entries')
        capture(dashboard,'Expand when you need the detail', 'The parent and its children stay together.', key='Right', duration=850)
        keys(dashboard,'Right');wait_text(dashboard,'Tokens  2400')
        capture(dashboard,'Move through the tree', 'Right selects the first child.', key='Right', duration=700)
        keys(dashboard,'/')
        capture(dashboard,'Find the session by its title', 'Search without leaving the dashboard.', key='/', duration=650)
        for letter in 'titles':
            tmux('send-keys','-l','-t',dashboard,letter);settle(.1)
            capture(dashboard,'Find the session by its title', 'Type a few letters to narrow the list.', key='type: titles', duration=90)
        keys(dashboard,'Enter');wait_text(dashboard,'Polish session titles')
        assert 'Verify terminal cleanup' not in pane_text(dashboard), 'search did not filter the sibling'
        capture(dashboard,'Keep the matching session in view', 'Search matches titles, workspaces and session metadata.', key='Enter', duration=1500)
        keys(dashboard,'c');wait_text(dashboard,'Synthetic demo request')
        capture(dashboard,'Read the latest local messages', 'A bounded excerpt, opened on request; never exported in JSON.', key='c', duration=2200, png='conversation-preview.png')
        keys(dashboard,'Escape');wait_text(dashboard,'3 shown / 3 entries')
        capture(dashboard,'Close the excerpt and clear the search', 'Conversation text is cleared when the view closes.', key='Esc', duration=1200)
        keys(dashboard,'Home');wait_text(dashboard,'Tokens  14200')
        keys(dashboard,'d');wait_text(dashboard,'Details')
        assert 'demo-mac' in pane_text(dashboard), 'detail hostname was not isolated'
        capture(dashboard,'Inspect the evidence when it matters', 'PID, parent relationship and navigation target remain inspectable.', key='d', duration=2100)
        keys(dashboard,'Escape')
        capture(dashboard,'Back to the session overview', 'Technical details stay out of the main view.', key='Esc', duration=800)
        keys(dashboard,'p');wait_text(dashboard,'TTYbird demo workspace')
        capture(dashboard,'Preview without switching terminals', 'tmux captures the pane; libghostty-vt parses its text and colors.', key='p', duration=2200, png='terminal-preview.png')
        # This signal makes our owned synthetic process print, not an agent-input action.
        os.kill(fixture_pid,signal.SIGUSR1)
        wait_text(dashboard,'Preview updated from the original pane.')
        capture(dashboard,'Watch the preview refresh', 'New output appears from the source pane. TTYbird sends no input.', duration=2000)
        keys(dashboard,'p')
        capture(dashboard,'Keep your existing terminal workflow', 'Close the preview; the original pane keeps running.', key='p', duration=1100)
        keys(dashboard,'?');wait_text(dashboard,'Help  ?')
        capture(dashboard,'The controls are always one key away', 'Tree navigation, search, conversation, preview and terminal focus.', key='?', duration=1800)
        keys(dashboard,'Escape')
        capture(dashboard,'Ready to return to the original terminal', 'Enter uses the verified mapping, not a workspace-name guess.', key='Enter next', duration=1500)
        keys(dashboard,'Enter');settle(.5)
        active=tmux('display-message','-p','-t','recording','#{pane_id}').decode().strip()
        assert active==agent, 'Enter did not focus the exact synthetic agent pane'
        capture(agent,'You are back in the original tmux pane', 'This step really changes the active pane; it does not start a new agent.', key='Enter', duration=2100)
        capture(agent,'Try it: ttybird --local', 'Actual TTYbird interaction. Synthetic data. No model or API calls.', duration=2000)
    finally:
        subprocess.run(['tmux','-L',SERVER,'kill-server'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        if client is not None:
            try:client.wait(timeout=3)
            except subprocess.TimeoutExpired:client.kill();client.wait()
        for fd in (master,slave):
            if fd is not None:os.close(fd)

frames[0].save(ROOT/'docs/walkthrough.gif',save_all=True,append_images=frames[1:],duration=[item['duration_ms'] for item in metadata],loop=0,optimize=True)
with Image.open(ROOT/'docs/walkthrough.gif') as image:
    assert image.n_frames==len(frames)
    for i in range(image.n_frames):image.seek(i);image.load()
(ROOT/'docs/demo-media.json').write_text(json.dumps({'source':'Actual compiled TTYbird TUI on a private tmux server, synthetic writable logs and agent process','real_agent_content_exported':False,'real_tmux_focus_performed':True,'ghostty_gui_focus_performed':False,'frames':metadata,'duration_ms':sum(item['duration_ms'] for item in metadata),'timing':'Edited playback: short navigation beats and longer explanatory holds; not a wall-clock performance measurement','reproduce':'cargo build --release --locked --bin ttybird --example capture_frame && uv run --with pillow scripts/record_demo.py'},indent=2)+'\n')
print(f"Recorded {len(frames)} frames / {sum(item['duration_ms'] for item in metadata)/1000:.2f}s; privacy and exact tmux focus passed.")
