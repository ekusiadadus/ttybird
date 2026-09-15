"""Render synthetic `cargo run --example tui_preview` JSON; requires Pillow.

Usage: uv run --with pillow scripts/render_preview.py CELLS.json OUTPUT.png
"""
import json
import sys
from PIL import Image, ImageDraw, ImageFont

data = json.load(open(sys.argv[1]))
font = ImageFont.truetype('/System/Library/Fonts/Menlo.ttc', 14)
cw, ch, pad = 9, 21, 18
image = Image.new('RGB', (data['width'] * cw + pad * 2, data['height'] * ch + pad * 2), '#101820')
draw = ImageDraw.Draw(image)
for c in data['cells']:
    x, y = pad + c['x'] * cw, pad + c['y'] * ch
    draw.rectangle((x, y, x + cw, y + ch), fill=c['bg'])
    draw.text((x, y), c['text'], fill=c['fg'], font=font)
image.save(sys.argv[2])
