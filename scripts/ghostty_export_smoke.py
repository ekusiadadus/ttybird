#!/usr/bin/env python3
"""Manual native Ghostty test; only creates/captures/closes synthetic terminals.
Run via the clipboard guard: swift scripts/ghostty_export_clipboard_guard.swift
  python3 scripts/ghostty_export_smoke.py PATH_TO_TEST_BINARY EVIDENCE_JSONL
Evidence contains booleans/counts, never screen or clipboard bodies.
"""
import datetime
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def apple(body):
    result = subprocess.run(['/usr/bin/osascript', '-e', 'tell application "Ghostty"\n' + body + '\nend tell'], capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError('fixture AppleScript failed: ' + result.stderr[:300])
    return result.stdout.strip()


def main():
    binary = str(Path(sys.argv[1]).resolve())
    evidence = Path(sys.argv[2])
    evidence.parent.mkdir(parents=True, exist_ok=True)
    owned = []
    repo = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(prefix='ttybird-export-fixture-') as temp:
        root = Path(temp)
        def record(checkpoint, **values):
            with evidence.open('a') as output:
                output.write(json.dumps({'at': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'checkpoint': checkpoint, **values}) + '\n')
        def config(marker):
            command = f'{sys.executable} {repo}/scripts/ghostty_ax_fixture.py {root} TTYbird-export-fixture {marker}'
            assert not any(c in command for c in ['"', '\\', '\n'])
            return f'set cfg to new surface configuration\nset command of cfg to "{command}"\nset initial working directory of cfg to "{root}"\nset wait after command of cfg to false\n'
        def ready(marker):
            deadline = time.monotonic() + 8
            while not (root / f'initial-rows-{marker}').exists():
                if time.monotonic() > deadline:
                    raise RuntimeError('fixture startup timeout')
                time.sleep(.05)
        def capture(checkpoint, ident, marker, closed=False):
            before = apple(f'return id of selected tab of window id "{window}"')
            env = dict(os.environ, TTYBIRD_TEST_GHOSTTY_UUID=ident, TTYBIRD_TEST_GHOSTTY_MARKER=marker)
            if closed:
                env['TTYBIRD_TEST_GHOSTTY_CLOSED'] = '1'
            temp_root = Path(tempfile.gettempdir())
            def exports():
                return {str(p) for p in temp_root.glob('*/screen.txt') if re.fullmatch(r'[A-Za-z0-9_-]{22}', p.parent.name)}
            before_files = exports()
            start = time.monotonic()
            result = subprocess.run([binary, '--ignored', '--nocapture', 'synthetic_native_snapshot'], env=env, capture_output=True, text=True, timeout=20)
            after = apple(f'return id of selected tab of window id "{window}"')
            cleaned = not (exports() - before_files)
            record(checkpoint, passed=result.returncode == 0 and before == after and cleaned, temporary_exports_cleaned=cleaned, selected_tab_unchanged=before == after, elapsed_ms=round((time.monotonic()-start)*1000), test_exit=result.returncode)
            if result.returncode:
                print(result.stdout + result.stderr, file=sys.stderr)
                raise RuntimeError('native snapshot assertion failed')
            assert before == after, 'export changed selected tab'
            assert cleaned, 'export file remained'
        try:
            created = apple(config('FIRST') + 'set w to new window with configuration cfg\nreturn (id of w) & "|" & (id of terminal 1 of selected tab of w)')
            window, first = created.split('|')
            owned.append(first)
            ready('FIRST')
            capture('E01-selected-tab', first, 'AX_FIRST')
            second = apple(config('SECOND') + f'set t to new tab in window id "{window}" with configuration cfg\nreturn id of terminal 1 of t')
            owned.append(second)
            ready('SECOND')
            (root / 'phase').write_text('HIDDEN')
            deadline = time.monotonic() + 5
            while not (root / 'ack-FIRST').exists() or (root / 'ack-FIRST').read_text() != 'HIDDEN':
                if time.monotonic() > deadline: raise RuntimeError('hidden output timeout')
                time.sleep(.05)
            capture('E02-hidden-tab-new-output', first, 'AX_HIDDEN_FIRST')
            apple(f'focus terminal id "{first}"')
            split = apple(config('SPLIT') + f'set s to split terminal id "{first}" direction right with configuration cfg\nreturn id of s')
            owned.append(split)
            ready('SPLIT')
            capture('E03-original-after-split', first, 'AX_FIRST')
            capture('E04-exact-split', split, 'AX_SPLIT')
            apple(f'close terminal id "{split}"')
            capture('E05-closed-uuid', split, 'AX_SPLIT', True)
        finally:
            (root / 'phase').write_text('EXIT')
            time.sleep(.3)
            for ident in owned:
                apple(f'if exists terminal id "{ident}" then close terminal id "{ident}"')
            time.sleep(.3)
            remaining = 0
            for pid_file in root.glob('pid-*'):
                try: os.kill(int(pid_file.read_text()), 0)
                except ProcessLookupError: continue
                remaining += 1
            record('E06-cleanup', passed=remaining == 0, fixture_processes_remaining=remaining)
            assert remaining == 0, 'synthetic process remained'
    print('Synthetic Ghostty UUID export scenarios passed; evidence: ' + str(evidence))

if __name__ == '__main__':
    main()
