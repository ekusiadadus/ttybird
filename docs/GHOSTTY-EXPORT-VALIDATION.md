# Existing Ghostty output snapshots

Verified 2026-09-16 on macOS, Ghostty 1.3.1 (15212), local TTYbird 0.10.2 development build. [Machine-readable evidence](ghostty-export-evidence.json) contains timestamps and source hashes, without terminal or clipboard contents.

## User operation

Explicitly map the correct Ghostty pane using `g` if necessary. Selecting a mapped local terminal displays it directly. `r` captures again; `p`/Esc closes the snapshot. `p` reopens it. Enter still focuses the existing terminal. No tmux or agent restart is required. Only a verified local process with a saved exact UUID binding is supported.

This is a VT **output snapshot**, including retained scrollback. TTYbird renders it at 120 columns, keeping up to the latest 200 rows. It does not preserve the exact original viewport, cursor, wrapping or scroll position. No automatic polling occurs on this route.

## Chronological checkpoints (JST)

- 15:29: initial selected-tab export and libghostty rendering passed. The harness then failed because it read its acknowledgement file before creation. Cleanup succeeded; this was not a complete successful run.
- 15:29: corrected harness passed selected tab, updated hidden tab, original pane after split, exact new split, and closed UUID rejection. Temporary files removed and synthetic process count returned to zero.
- 15:31: independent review prompted use of `parse_vt` rather than tmux-specific LF normalization, an exact closed-UUID error assertion, and explicit timeout limitations. Repeated native scenarios passed.
- Subsequent final run: all E01–E07 checkpoints in the linked JSON passed, including byte-for-byte readback of the original clipboard representations after restoring them. Raw terminal/clipboard contents were not logged.
- Final checks: 152 Rust tests passed; format and Clippy all-target checks passed. The separate native tmux integration passed. A real PTY with synthetic discovery/AppleScript transport verifies selection opens without `p`, `r` exports once, hiding preview stops captures, reopening captures once, failures do not auto-retry or focus, and the dashboard remains open. Local read-only JSON collection passed.

The native driver uses the real Ghostty application and the real Rust adapter plus libghostty parser. The PTY UI transport test uses a fake AppleScript endpoint; it does not by itself prove GUI focus. Native checks compare selected tab before/after; they do not independently observe internal split-focus changes.

## Clipboard and cleanup boundaries

Ghostty's `write_screen_file:copy,vt` temporarily puts the generated file path on the general clipboard. The adapter keeps up to 4 MiB of clipboard representations in memory, checks the expected change count and exact private export path, and restores only when its export still appears current. TTYbird export requests are serialized with a per-user file lock. Owned regular files are read with a 1 MiB bound and removed, including oversize rejection. Symlinks and unexpected paths are rejected without removing unrelated files.

There is no atomic pasteboard compare-and-restore operation. A concurrent copy in the final check/restore gap cannot be ruled out. Helper termination, a crash, or a 10-second timeout can leave a filepath on the clipboard or a private export file. Normal dashboard exit waits for bounded cleanup; SIGKILL cannot guarantee cleanup. Consequently, this feature captures only when the user selects/opens a preview or refreshes it; it does not poll. The clipboard side effect is documented here and in the README. No clipboard backup is persisted.

## Reproduce

Use a quiet clipboard and a native macOS Ghostty installation. This opens and closes only synthetic fixture tabs. Rust native tests are ignored during normal `cargo test` to avoid unexpected GUI/clipboard operations.

```sh
cargo test --release --locked --test ghostty_export --no-run
# Replace TEST_BINARY with the executable path printed above.
env -u SDKROOT DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  /usr/bin/swift scripts/ghostty_export_clipboard_guard.swift \
  python3 scripts/ghostty_export_smoke.py TEST_BINARY /tmp/ttybird-export-checks.jsonl
python3 scripts/ghostty_picker_smoke.py target/release/ttybird
```

The wrapper preserves the original clipboard in memory, seeds two synthetic representations, verifies them after the run, and restores/readbacks the original only if no unrelated copy replaced the seed. Run after building with the repository's pinned Zig toolchain.
