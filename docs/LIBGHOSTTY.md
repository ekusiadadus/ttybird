# libghostty in TTYbird

This note records the upstream state checked on 2026-09-16 and separates
terminal emulation from terminal capture and Ghostty application control.

## Versions and API boundaries

- The latest tagged Ghostty release is
  [1.3.1, released 2026-03-13](https://ghostty.org/docs/install/release-notes/1-3-1).
  The current upstream `main` revision checked for this note is
  [`d4c88d8069912b653d707191388ca98e24751f12`](https://github.com/ghostty-org/ghostty/commit/d4c88d8069912b653d707191388ca98e24751f12),
  dated 2026-09-15. APIs described below as `main` are unreleased and may
  change.
- TTYbird uses the community Rust binding
  [`libghostty-vt` 0.2.1](https://crates.io/crates/libghostty-vt/0.2.1),
  which is still the newest crates.io version. Its sys crate pins Ghostty
  [`a887df42c56f6de86c0fe6da9c4eeca37931e083`](https://github.com/ghostty-org/ghostty/commit/a887df42c56f6de86c0fe6da9c4eeca37931e083),
  dated 2026-07-11. This is post-1.3.1 development code, not the 1.3.1
  `libghostty-vt` surface.
- The Rust binding's current `master` revision is
  [`5988a0b78b4aa804d1c12e66bbfe662bd97d81c0`](https://github.com/Uzaaft/libghostty-rs/commit/5988a0b78b4aa804d1c12e66bbfe662bd97d81c0),
  dated 2026-09-01. It still declares version 0.2.1 but pins a newer Ghostty
  revision and adds an unreleased safe snapshot module. It does not yet expose
  high-level Rust modules for every C API now on Ghostty `main`, notably search
  and formatter.
- [`libghostty-vt`](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt.h)
  is the public terminal-emulation C API. It parses byte streams, maintains
  terminal state, exposes cells and styles, and encodes input events. Upstream
  explicitly calls it incomplete and pre-stable.
- [`include/ghostty.h`](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty.h)
  is `libghostty-internal`, the macOS application's private embedder API.
  Upstream says it is tailored to the macOS app and directs external embedders
  to `libghostty-vt`. Although it contains surface text and input functions,
  it is not an API for attaching to a separately running Ghostty process.
- [Ghostty AppleScript](https://ghostty.org/docs/features/applescript) is a
  separate macOS automation API for enumerating windows, tabs, and terminal
  surfaces and focusing an exact surface ID. It does not expose terminal screen
  text.

The 1.3.1 source only exposes the earlier, smaller `libghostty-vt` header set.
The modular terminal, render, formatter, snapshot, and search surface visible
in the current [`vt.h`](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt.h)
has continued to grow on `main`. A Ghostty application version and a
`libghostty-vt` API revision therefore must not be treated as interchangeable.

## What TTYbird uses today

`src/terminal_preview.rs` validates a live local process identity, its TTY, an
exact tmux pane ID, pane liveness, and stable dimensions around a bounded
`tmux capture-pane -p -e -N` call. It captures exactly the visible rows and
rejects malformed framing. The capture and the two metadata queries are not an
atomic tmux snapshot; output may change between them.

`src/preview.rs` creates a short-lived `libghostty-vt` terminal with zero
scrollback, normalizes tmux row delimiters, feeds at most 1 MiB into at most a
500 by 200 cell screen, and converts the render snapshot into owned Ratatui
text. It keeps all native handles on one worker invocation because the Rust
handles are intentionally `!Send` and `!Sync`.

The APC buffer is explicitly capped at 4 KiB and the unused Glyph Protocol is
disabled. No effect callback is installed. Consequently VT requests that need a host
side effect or a reply are not sent anywhere: there is no PTY write, clipboard
write, bell, notification, title action, or external command. TTYbird returns
owned text and never replays captured escape sequences into its own terminal.

The current conversion preserves:

- Unicode grapheme clusters and wide-cell layout;
- indexed and RGB foreground, background, and underline colors;
- bold, faint, italic, underline, blink, reverse, hidden, and strike flags;
- cursor movement, erasure, alternate-screen behavior, wrapping, and other
  terminal state needed to reconstruct the captured rows.

It is not pixel-equivalent to the Ghostty GUI. Ratatui has no representation
for Ghostty's distinct underline shapes or overline, so underline shapes are
collapsed and overline is omitted. Default colors inherit the surrounding TUI
instead of reproducing Ghostty's configured theme. Fonts, shaping, ligatures,
images, shaders, and GUI cursor rendering are outside this text renderer.

## Capability matrix

| Capability | Upstream library state | TTYbird state and practical limit |
|---|---|---|
| VT parsing and styled cells | Available in the pinned Rust API | Used for visible tmux capture |
| Scrollback and viewport scrolling | Available in the pin; upstream also supports reflow and compression | Disabled with `max_scrollback: 0`; only visible tmux rows are captured |
| Selection and copy formatting | Line, word, output, rectangle, gesture, and formatting APIs exist in the pin | Native terminal is dropped after each parse, so no selection is retained |
| Text search | Current Ghostty `main` has a [bounded, incremental search API](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt/search.h) across active and alternate screens and scrollback | Not in released Rust 0.2.1's safe API; there is also no captured history to search today |
| Hyperlinks | The pin can query hyperlink URIs from terminal grid references | Ratatui `Text` has no link target, and tmux `capture-pane -e` promises text/background attributes rather than the original OSC 8 stream, so targets cannot be claimed from the current capture |
| Cursor state | Render snapshots expose visibility, style, color, and viewport position | Replaying a flattened capture creates a parser cursor at the replay endpoint, not necessarily tmux's real cursor. Use separately queried tmux cursor coordinates before displaying one |
| Keyboard input | Key encoder handles application modes, modifyOtherKeys, and Kitty keyboard flags | Encoding does not deliver bytes to an existing pane. TTYbird intentionally has no agent-input path |
| Mouse and focus input | Mouse encoders cover X10, UTF-8, SGR, urxvt, and SGR-pixels; focus encoding is available | Useful only when an embedder owns the PTY write side. It does not control Ghostty GUI focus |
| Paste | Safety checks, bracketed-paste encoding, and terminal-aware paste APIs exist | TTYbird does not write to agent terminals |
| Terminal replies and effects | The pin can register synchronous callbacks for PTY replies, bell, enquiry, version, title, working directory, size, color scheme, device attributes, and clipboard writes | Deliberately unregistered for read-only preview; queries and side effects remain inert |
| Kitty graphics and other images | Optional library data and placement APIs exist; current `main` is richer | A tmux text capture does not provide the original image payload and Ratatui `Text` cannot render it |
| Formatter | Current `main` formats terminal state as plain text, VT, or HTML | No released high-level Rust wrapper; the current per-cell conversion is needed for Ratatui styles |
| Snapshots | Current `main` has a CRC-protected incremental terminal snapshot format; Rust `master` has an unreleased safe wrapper | Useful for an owned, long-lived emulator stream, not for recovering state absent from a tmux capture |
| Full terminal embedding | `libghostty-vt` supplies emulation but no PTY process, event loop, renderer, or GUI | Out of scope for a dashboard preview |
| Existing Ghostty surface text | Private `libghostty-internal` can read a surface inside its embedder process | No supported out-of-process screen capture API |

The tmux contract is the limiting input contract. The
[`capture-pane` manual](https://man.openbsd.org/tmux#capture-pane) says `-e`
includes escape sequences for text and background attributes. It can capture
history with negative `-S` positions, but it is not the original PTY byte
stream and does not promise cursor, reply, image, or hyperlink payloads.

## Practical next functions

1. **Bounded recent history and preview search.** Capture a configured number
   of recent tmux history rows, retain a bounded emulator scrollback, and add
   preview scrolling. Search can initially operate on TTYbird's owned text and
   highlight map. Adopting Ghostty `main`'s search API should wait for a
   released compatible Rust wrapper; it is most valuable for a long-lived
   terminal whose scrollback changes, while TTYbird currently rebuilds a small
   snapshot for each refresh.
2. **Accurate cursor metadata.** Add `#{cursor_x}` and `#{cursor_y}` to both
   pane metadata checks and return cursor data beside `Text`. Do not use the
   replay parser's endpoint as the source of truth. Visibility/style still may
   require more tmux metadata and should be omitted unless verified.
3. **Selectable preview text.** Keep selection coordinates and copied text in
   TTYbird-owned data. The pin's selection formatter is useful if the native
   terminal remains alive for the interaction; otherwise a Ratatui-level
   selection avoids keeping `!Send` handles across UI tasks.
4. **Theme defaults.** Pass explicit preview foreground/background and palette
   defaults if product design wants deterministic colors. This improves
   consistency but still cannot reproduce Ghostty fonts and GUI rendering.
5. **Parser hardening (implemented during this audit).** Keep the existing byte,
   dimension, and timeout limits. The parser now explicitly caps APC buffering
   at 4 KiB and disables Glyph Protocol handling. The control-string regression
   also verifies that text after an oversized APC remains visible. This does
   not claim that every graphics protocol is disabled.

Hyperlink activation, image rendering, and terminal input should not be added
on the strength of `libghostty-vt` alone. They require a capture transport that
preserves the source data and a separately designed authorization and routing
path.

## Ghostty surfaces, remote panes, and navigation

The installed Ghostty.app was also checked: version 1.3.1, with no `pid` or
`tty` properties in its bundled scripting dictionary.

Ghostty 1.3.1 AppleScript exposes stable terminal IDs, titles, working
directories, and `focus`, but not TTY or PID properties. Current `main` added
read-only `pid` and `tty` properties in
[`9a9002202b8767e6e99c2bb48fad09fc0ae02870`](https://github.com/ghostty-org/ghostty/commit/9a9002202b8767e6e99c2bb48fad09fc0ae02870)
after 1.3.1. When those properties reach a tagged release, TTYbird can
version-gate exact local TTY-to-surface discovery and keep the explicit picker
as a fallback. A working-directory match remains ambiguous. The reported PID
is the foreground process and is not, by itself, a stable descendant-session
identity.

AppleScript `focus` is application navigation; AppleScript `input text`, key,
and mouse commands are separate write operations and are not needed by
TTYbird. Neither AppleScript nor `libghostty-vt` supplies the bytes shown in an
already running Ghostty surface.

For remote hosts, `libghostty-vt` can parse bytes after TTYbird obtains them,
but it provides no transport or remote pane discovery. A future remote preview
would need bounded SSH execution of tmux metadata and capture commands, exact
remote host/process/pane identity checks, and the same stale-result rejection.
It cannot preview a generic remote PTY that TTYbird does not own and that is
not exposed through a multiplexer or another explicit capture service.

## Build, license, and upgrade policy

The released Rust crates are licensed MIT OR Apache-2.0; Ghostty is MIT.
TTYbird statically links the vendored native library, so end users do not need
Ghostty.app or a shared `libghostty-vt` at runtime. The pin requires Rust 1.90
or newer and Zig 0.15.2. Without source overrides, the sys build invokes Git to
fetch the exact Ghostty commit and Zig resolves its native packages. TTYbird's
Nix package instead supplies the pinned Ghostty source and an offline Zig
package cache.

The static native build includes dependencies that do not appear in Cargo's
dependency inventory. TTYbird packages provenance and license texts for the
pinned uucode 0.2.0, simdutf 5.2.8, and Highway revision under
`licenses/native/`, in addition to Ghostty's license.

Do not point 0.2.1 at an arbitrary installed or current `libghostty-vt`.
Both upstream and the binding describe the API as pre-stable, and the header
surface has changed substantially since the current pin. An upgrade should pin
one Rust release and its exact Ghostty revision, regenerate the offline Zig
dependency set and license inventory, run the parser's synthetic control and
Unicode cases, run the real private-tmux capture test, and verify macOS and
Linux package builds independently.
