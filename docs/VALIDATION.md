# Validation

Validation is about user-visible failure boundaries, not test count or a
coverage target. [TEST-AUDIT.md](TEST-AUDIT.md) inventories the tests and the
real regressions each retained group protects.

## Reproduce

Use `nix develop` or Rust + Zig 0.15.2. From the repository root:

```sh
cargo fmt --check
cargo clippy --release --locked --all-targets -- -D warnings
cargo test --release --locked -- --test-threads=2
cargo build --release --locked --bin ttybird
LC_ALL=C cargo test --release --locked --test tmux_preview -- --ignored
python3 scripts/liveness_smoke.py target/release/ttybird
python3 scripts/tui_smoke.py target/release/ttybird
LC_ALL=C python3 scripts/tmux_preview_smoke.py target/release/ttybird --gemini
# macOS only; uses an osascript stub, never focuses a real pane:
python3 scripts/ghostty_picker_smoke.py target/release/ttybird
```

The integration fixtures own temporary processes, PTYs and a private tmux
server, and remove them afterward. `cc`, `python3`, `node` and `tmux` must be on
PATH for the corresponding scripts. No provider account/API call is used.
Ordinary Rust tests keep the tmux integration ignored so it can be run
explicitly; CI runs it in addition to the ordinary suite.

## What these checks establish

| Boundary | Evidence | What is not established |
|---|---|---|
| Process/session identity | Real synthetic live process, descriptor close, stale binding, zombie and exit; comparison with `ps` | Every provider's live API state; atomic OS snapshot |
| Parent/child display | Scoped links, cycle handling, folds, filter and selection transitions | Independently executing model calls for each visible child |
| VT conversion | Pinned native libghostty-vt parses captures; owned text/styles are rendered by Ratatui | Full Ghostty GUI renderer equivalence, arbitrary PTY streams or graphics |
| tmux preview | Real private pane; colors/Unicode/CR/erase, alternate screen, wrong identity, unchanged pane/buffers | Arbitrary tmux versions and remote capture |
| Dashboard | Real PTY, resize, q/Ctrl-C/SIGTERM and incomplete-input termination, terminal configuration/status-flag restoration, PTY hangup at multiple timings, preview close/reopen | Exhaustive terminal emulators/keymaps |
| Ghostty chooser | Real TUI with synthetic discovery, cancellation/stale rejection and exact-ID focus invocation | A GUI end-to-end focus test on every Ghostty version |
| SSH collector | Fixed command, versioned protocol, invalid destination rejection, bounded child execution | Authenticated multi-host network test |
| Release portability | CI-native build and library reference check, version execution before packaging | Notarization, signing, all macOS/Linux versions |

The [Rust workflow](../.github/workflows/ci.yml) runs on Apple Silicon macOS and
x86_64 Ubuntu. The [Nix workflow](../.github/workflows/nix.yml) checks the pinned
package separately. A declared target or a local evaluation is not proof of a
completed CI run; see the actual Actions result for the published commit.

The dashboard hangup regression closes the **dashboard's own** PTY, both with
normal and initially ignored SIGHUP. This is separate from the liveness test,
which deliberately keeps a synthetic agent alive after its terminal closes.
Each dashboard must exit within two seconds and leave no fixture process group.
The restoration checks compare configurable termios fields and mutable file
status flags; macOS `PENDIN` and the kernel's `FWASWRITTEN` bookkeeping are not
configuration leaks. Fixtures never send input to real agent terminals.

## Optional real Ghostty GUI check

On macOS with Ghostty running, explicitly opt in:

```sh
python3 scripts/ghostty_focus_smoke.py target/release/ttybird --allow-gui
```

This creates two **test-owned** windows with synthetic processes, binds/focuses
through the real CLI, checks that a killed fixture cannot be focused, closes
only those windows, and restores the original focused terminal. It never sends
input to existing sessions. This GUI test is intentionally not a headless CI job.

Local result on 2026-09-15: **one unexplained failure followed by one pass**.
The first failure was after successful creation/discovery/binding; its exact
focus phase was not recorded. Both attempts cleaned up and restored the original
terminal. The second observed the requested UUID, rejected the dead fixture and
preserved the other window. This is evidence that the path works, not a stability
or universal correctness guarantee. Current diagnostics record each focus stage
as booleans, without recording private terminal metadata.

## Native parser boundary

The community [Rust binding](https://github.com/uzaaft/libghostty-rs) is pinned
to 0.2.1. Its sys crate builds [Ghostty at the pinned revision](https://github.com/ghostty-org/ghostty/tree/a887df42c56f6de86c0fe6da9c4eeca37931e083)
using Zig 0.15.2. TTYbird keeps the native handle on one capture worker and
exports only owned text/styles. No clipboard/write callbacks are installed and
captured escape sequences are not replayed to the user's terminal.

Preview is a bounded visible-screen snapshot: terminal identity can change
concurrently, so a failed recheck discards the result. It is not a stream and
cannot guarantee observing every intermediate screen. Ratatui supports only a
subset of the native renderer's presentation (for example, graphics and exact
underline shapes are outside the UI contract).

[Ghostty's AppleScript API](https://ghostty.org/docs/features/applescript)
controls GUI focus separately from libghostty-vt. No amount of VT parsing can
supply a missing mapping from a Ghostty surface to a process.

## Public screenshots

The main screenshots and [walkthrough GIF](walkthrough.gif) record the actual
compiled TUI on a private tmux server. Synthetic Codex processes keep writable
fixture logs open; scoped discovery excludes all unrelated processes. No
model/API is called. Session titles, usage, tree navigation, opt-in conversation,
libghostty preview and Enter navigation run through the application.

```sh
cargo build --release --locked --bin ttybird --example capture_frame
uv run --with pillow scripts/record_demo.py
```

The recording uses English headlines, explanations and key badges. Navigation
beats are short; explanatory holds are longer. The 26-frame, 30.14-second
walkthrough includes search, conversation, evidence, a live preview refresh and
terminal return. Edited playback timing is not a performance measurement.

The script isolates the displayed hostname and process inventory, checks
synthetic inventory counts and rendered text before export,
checks the exact active pane after Enter, and decodes every GIF frame. The
terminal capture is rendered into PNGs with libghostty-vt, Ratatui and Pillow
(macOS Menlo). This is a terminal recording, not a desktop pixel capture.
[demo-media.json](demo-media.json) records the scope: real private tmux focus,
no Ghostty GUI focus, no real user conversations.

The Ghostty chooser PNG remains an explicitly synthetic UI fixture. Static
layout fixtures can be regenerated under `target/demo-media` with
`cargo build --release --example tui_preview` and
`uv run --with pillow scripts/capture_demo.py`; they do not overwrite public recordings.
