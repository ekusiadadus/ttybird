"""Synthetic-only Ghostty AX integration fixture; no model or network calls."""
import os
from pathlib import Path
import sys
import time

root, title, marker = sys.argv[1:]
root = Path(root)
(root / f"pid-{marker}").write_text(str(os.getpid()))
print(f"\033]2;{title}\007", end="")
print("AX_OLDEST")
for i in range(160):
    print(f"AX_HISTORY_{i:03d}")
print(f"AX_{marker}")
print("日本語 e\u0301 😀 👩‍💻")
print("AX_INITIAL", flush=True)
(root / f"initial-rows-{marker}").write_text(str(os.get_terminal_size().lines))
last = ""
deadline = time.monotonic() + 90
while time.monotonic() < deadline:
    try:
        phase = (root / "phase").read_text()
    except FileNotFoundError:
        phase = ""
    if phase == "EXIT":
        break
    if phase != last:
        if phase == "ALT":
            print("\033[?1049h\033[2J\033[HAX_ALTERNATE", flush=True)
        elif phase == "NORMAL":
            print("\033[?1049l", end="", flush=True)
        elif phase == "HIDDEN":
            print(f"AX_HIDDEN_{marker}", flush=True)
        elif phase == "LARGE":
            for i in range(20000):
                print(f"AX_LOAD_{i:05d}_" + "x" * 60)
            print("AX_LARGE_END", flush=True)
        elif phase:
            print(f"AX_PHASE_{phase}", flush=True)
        (root / f"ack-{marker}").write_text(phase)
        last = phase
    time.sleep(0.01)
