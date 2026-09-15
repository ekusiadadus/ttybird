"""Reproduce public demo assets from synthetic fixtures and the real TUI renderer.

uv run --with pillow scripts/capture_demo.py [target/release/examples/tui_preview]
No live collector, terminal capture, provider, or network calls are made.
"""
import json
from pathlib import Path
import subprocess
import sys
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / 'target/release/examples/tui_preview').resolve()
OUT = ROOT / 'target/demo-media'
OUT.mkdir(parents=True, exist_ok=True)
# Static fixture renders are developer artifacts. record_demo.py owns public media.
DOCS = OUT
FONT = ImageFont.truetype('/System/Library/Fonts/Menlo.ttc', 16)
FORBIDDEN = ('toioswarm', 'isucon', 'rovnou', 'livepass', 'ekusiadadus', '/users/', '/home/')


def render(name, *args):
    raw = subprocess.check_output([str(BINARY), '160', '34', *args])
    data = json.loads(raw)
    rows = [''.join(c['text'] for c in data['cells'] if c['y'] == y) for y in range(data['height'])]
    visible = '\n'.join(rows).lower()
    assert not any(word in visible for word in FORBIDDEN), 'Private name/path in demo frame'
    cells = OUT / (name + '.json')
    cells.write_bytes(raw)
    image = OUT / (name + '.png')
    subprocess.run([sys.executable, str(ROOT / 'scripts/render_preview.py'), str(cells), str(image)], check=True)
    with Image.open(image) as source:
        source.load()
        return source.convert('RGB')


for name, args in {
    'tui-preview': ['--tree'],
    'terminal-preview': ['--tree', '--terminal'],
    'ghostty-picker': ['--tree', '--picker'],
}.items():
    render(name, *args).save(DOCS / (name + '.png'))

steps = [
    ('1  PARENT + CHILDREN', 'Astra parent and Sol children in the ttybird workspace'),
    ('2  SPACE', 'Collapse the parent branch'),
    ('3  RIGHT', 'Expand the branch again'),
    ('4  RIGHT', 'Select a child; inspect its model and parent relationship'),
    ('5  d', 'Open full session details'),
    ('6  Esc / p', 'Show synthetic terminal output through libghostty-vt'),
    ('7  GHOSTTY CHOOSER', 'Show the Ghostty chooser fixture; no terminal is focused'),
]
frames = []
for i, (key, caption) in enumerate(steps, 1):
    ui = render('step-' + str(i), '--step=' + str(i))
    frame = Image.new('RGB', (ui.width, ui.height + 96), '#101820')
    frame.paste(ui, (0, 96))
    draw = ImageDraw.Draw(frame)
    draw.text((20, 12), 'TTYbird  /  interactive walkthrough  /  synthetic demo', font=FONT, fill='#7e919d')
    draw.text((20, 42), key, font=FONT, fill='#39c5bb')
    draw.text((300, 42), caption, font=FONT, fill='#dce7ed')
    draw.line((20, 88, ui.width - 20, 88), fill='#41535c')
    frame.save(OUT / f'frame-{i:02}.png')
    frames.append(frame)
frames[0].save(DOCS / 'walkthrough.gif', save_all=True, append_images=frames[1:], duration=[2400,1800,1800,2400,2400,2800,2800], loop=0, optimize=True)
with Image.open(DOCS / 'walkthrough.gif') as gif:
    assert gif.n_frames == len(steps)
    for i in range(gif.n_frames):
        gif.seek(i)
        gif.load()
(DOCS / 'demo-media.json').write_text(json.dumps({
    'source': 'Synthetic session fixtures; real Ratatui rendering and App tree methods',
    'live_sessions_captured': False,
    'terminal_focus_performed': False,
    'frames': len(steps),
    'duration_ms': 16400,
    'visible_text_privacy_check': 'passed on every rendered cell row',
    'workspaces': ['ttybird', 'demo-api', 'demo-web', 'demo-docs', 'demo-infra'],
    'reproduce': 'cargo build --release --locked --example tui_preview && uv run --with pillow scripts/capture_demo.py',
}, indent=2) + '\n')
print('Generated 3 PNGs and 7-frame walkthrough.gif; visible text privacy checks passed.')
