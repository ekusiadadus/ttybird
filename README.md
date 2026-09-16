# TTYbird

Find your coding agents. Return to their terminals.

A Rust TUI that discovers existing coding-agent processes on macOS and Linux.
Keep your current launcher and terminal: TTYbird adds a searchable parent/child
view, explicit Ghostty/tmux navigation, and a read-only tmux preview powered by
**libghostty-vt**. Listing requires no daemon or hooks.

**Alpha.** A live PID is not proof that a model is working. Activity can be
inferred or unavailable; the UI shows that distinction. Ghostty pane mapping
requires your explicit choice the first time.

[日本語](README.ja.md) · [Architecture](docs/ARCHITECTURE.md) ·
[Provider coverage](docs/PROVIDERS.md) · [Tests and limits](docs/VALIDATION.md)

The walkthrough starts with the parent/child tree, recorded titles and token
metadata, then uses `/` search, `c` conversation, `p` read-only preview and `d`
evidence before **Enter** returns to the session's existing terminal.

![TTYbird walkthrough: session tree, titles, tokens, search, conversation, read-only preview, evidence and return to an existing terminal](docs/walkthrough.gif)

*The TTYbird interaction and tmux focus are real; the agent processes, session
data and messages are synthetic. It was recorded on a private tmux server, does
not show Ghostty GUI focus and calls no model or API. See [capture evidence](docs/demo-media.json).*

## Install and run

With Homebrew (Apple Silicon macOS 14+, or x86_64 Linux with glibc 2.39+):

```sh
brew install ekusiadadus/tap/ttybird
ttybird --local
```

The tap installs the tested release binary; no Rust or Zig build is needed.
Install `tmux` separately if you want tmux navigation and previews.

Download the archive for your platform from [Releases](https://github.com/ekusiadadus/ttybird/releases).
Alpha archives target Apple Silicon macOS 14+ and x86_64 Linux with glibc 2.39+
(Ubuntu 24.04+). Other targets require a source/Nix build.
The binary statically includes Ghostty's VT parser; Rust, Zig and Ghostty.app
are not required to run the collector. `ps` and, on macOS, `lsof` must be on
`PATH`; tmux/SSH are needed only for their respective integrations.

```sh
# Extract the downloaded archive, then:
./ttybird --local
```

Select a live session and press **Enter**. A mapped tmux pane opens directly;
an unmapped local session on macOS opens a **Ghostty pane chooser**. Choose the
matching pane yourself. Subsequent navigation uses that saved mapping while
its PID and start time still match. Ghostty 1.3+ and its AppleScript integration
are required for Ghostty navigation; macOS may request Automation permission.
Ghostty focus leaves TTYbird running in its original pane. Return to that pane
to select another agent; `q` closes the dashboard.

For a split view, press `p`: the agent list stays on the left and the selected
local tmux pane's screen appears on the right, refreshing every two seconds.
This preview is read-only; Enter returns to the actual terminal for typing.
It needs a window at least 105 columns wide (narrower windows stack the views).
Plain Ghostty tabs do not expose a supported live screen-reading API; run the
agent inside tmux in Ghostty to use the preview. Unsupported selections keep
the session summary visible. For a new embedded terminal without tmux, use
`ttybird run -- codex`. A failed or outdated Ghostty binding can be replaced
with `g`; select the correct pane explicitly.

Nix users can build the pinned package or enter the dev shell; see [Nix](docs/NIX.md).
For a source build, install Rust 1.90+ and **Zig 0.15.2**:

```sh
git clone https://github.com/ekusiadadus/ttybird.git
cd ttybird
cargo install --path . --locked
ttybird --local
```

The first source build downloads pinned Ghostty source and Zig dependencies.
The Rust binding is pinned to `libghostty-vt = 0.2.1`; Zig 0.16 is incompatible.

## Owned terminals (no tmux required)

Launch a new session explicitly to use a live, interactive right pane:

```sh
ttybird run --name backend -- codex
# Or: ttybird run -- claude
# Launch without opening the dashboard:
ttybird run --detach --name review -- codex
ttybird sessions
ttybird attach SESSION_ID
ttybird stop SESSION_ID
```

The agent list stays on the left. **Enter or i** puts the selected owned terminal
in **INPUT** mode; keys, Ctrl-C and pasted text go to that program. **Ctrl+]**
returns to the list. **q in the list** detaches without stopping the program.
Use `stop` to end an owned session. This does not import already-running Ghostty
tabs: launch through `run` when you want embedded display and input.

One local helper per owned session retains the PTY and bounded libghostty-vt
screen in memory while dashboards are closed. A private Unix socket carries
screen snapshots and explicit input. Only identity metadata is saved to disk;
screen contents and command arguments are not logged. Host reboot/helper failure
does not preserve a session. Multiple viewers share one PTY size; the most recent
viewer resize applies. Embedded mouse input and terminal graphics are not supported.

## Controls

| Key | Action |
|---|---|
| ↑ / ↓ or j / k | Select a session |
| Space / ← / → | Fold, expand, or navigate the parent/child tree |
| Enter | Open its terminal; a child without one uses its recorded parent |
| g | Relink the local Ghostty pane, including a child’s parent terminal |
| Enter / i on an owned terminal | Enter INPUT mode in the right pane |
| Ctrl+] in INPUT mode | Return to the list without stopping the program |
| p | Toggle local tmux preview; PageUp/PageDown scroll the captured screen |
| c | Recent local Codex/Claude messages, only when requested; Esc closes |
| d | Full metadata and evidence |
| H | Prepare and review a handoff before starting a new Codex session |
| N | Open the observed attention inbox; `m` acknowledges and `z` snoozes 10 minutes |
| / | Search title, workspace, provider, host or session |
| a | Only observed requests needing input |
| b / h | Show retained children, auxiliary and background processes / recent log-only history |
| r / ? | Refresh / keyboard help |
| q / Ctrl-C | Exit and restore the terminal |

`Parent` describes a relationship, not active work. For example,
a parent with no displayable children has no expand arrow or child count.
Space changes only branches that can reveal rows. `b` reveals retained children;
recorded relationships remain in Details.

For a child without its own terminal, Enter opens its **recorded parent's**
terminal. Only the parent needs the initial Ghostty mapping. `c` still opens the
selected child's conversation; the status line states the Enter destination.

Piped output is plain text by default. Use `--plain`, `--json`, or
`--watch --json` (JSONL) explicitly for scripts. `ttybird doctor` checks runtime
tools and `ttybird providers` reports adapter capabilities.

## Workspaces, attention and reviewed handoff

For sessions inside a Git checkout, TTYbird shows the checkout/worktree,
branch or detached HEAD, commit and dirty state. Inspection is read-only and
bounded; it disables optional Git locks and records changed path metadata, not
diff contents. Linked worktrees that share one repository remain distinct
checkouts.

Press **N** for the durable attention inbox. It accepts only current, observed
Claude hook evidence for permission/input requests, response completion and
tool failure. A normal `PreToolUse` event means a tool is running and is not an
approval request. **m** acknowledges the selected occurrence; **z** postpones
its reminder for 10 minutes. Reading an occurrence suppresses its current
desktop reminder, while a later observed occurrence opens a new item. A stale
or unreachable observation expires instead of being reported as stopped. An
explicit snooze keeps only a historical reminder until it is due; it does not
claim that the agent is still waiting. The same operations are available
without the TUI:

```sh
ttybird inbox
ttybird inbox ack EVENT_ID
ttybird inbox snooze EVENT_ID --minutes 10
```

Desktop notification delivery is off by default. Add `--notify` to the TUI or
watch mode to enable it. Notifications contain the event type and a sanitized,
truncated task title, never a transcript. Delivery is bounded and deduplicated
in the private config directory. It does not approve a request or send input to
an agent. “Response finished” describes one observed response; it does not prove
that the task is complete.

Press **H** to prepare and review a handoff to a new TTYbird-owned Codex session.
Preparation makes no model call. The draft contains checkout identity and
changed path metadata plus only the note files you select; a bounded recent
conversation excerpt is included only when explicitly requested. Review and
edit the private draft before starting it. The CLI form is:

```sh
ttybird handoff prepare [SESSION] --cwd PATH --note RELATIVE_PATH --include-conversation
ttybird handoff start BUNDLE --yes --detach
```

Use `--yes` only after reviewing the current `draft.md`; it is your explicit
confirmation to share that file. Omit it for an interactive review prompt.
Starting rechecks checkout, branch, HEAD and changed-path metadata, then launches
Codex with the existing local Codex defaults. File contents remain live.
The handoff does not stop or claim ownership of the source agent.

## What is actually observed

- Process identity uses **PID + start time**. Session metadata supplies parent
  IDs and recorded models. A parent and child can share one process and TTY.
- Codex/Claude use bounded local log samples; only optional Claude hooks
  currently supply observed input notifications. Other supported CLIs expose
  process metadata, with unknown activity. See [the adapter matrix](docs/PROVIDERS.md).
- `log only` is history, hidden by default. Open logs do not prove that a child
  is currently running. Children with idle, ended or unknown activity are also
  hidden by default; `b` reveals them without claiming they are working.
  [Liveness](docs/LIVENESS.md) explains the boundaries.
- `Ghostty binding` means a saved navigation target, not a confirmed open pane.
  The target is checked when you press Enter.
- Focus returns to the **hosting terminal**, not Codex's internal subagent view.
  TTYbird never infers a Ghostty binding from a working directory alone.

## Titles, tokens and conversation

The main view prioritizes provider-recorded session titles and token usage.
Missing metadata is shown as unavailable, never estimated from the model name.
Codex uses the latest reported cumulative token snapshot; repeated snapshots
are not added together. Claude usage is deduplicated by message ID and marked
`*` because it covers sampled messages, not a guaranteed complete session.
Counts include cached input and reasoning output in their respective totals;
they are not context-window size, a bill, or an aggregate of all children.

Press **c** for up to three recent local user/assistant messages. TTYbird
rechecks the process and transcript identity before reading. This view excludes
tools, system messages and reasoning; it stays in memory, is not exported in
JSON, and clears when closed or selection changes. Reopen it to refresh the
excerpt; it is a snapshot, not a live stream. Remote conversations and
providers without a verified transcript are unavailable.

When a session is present, raw processes of the same provider/host/workspace
are hidden by default and available with **b**. This is a display preference,
not an identity merge: no PID, TTY or navigation binding is copied between them.

## libghostty-vt preview

Press `p` on a session in a local tmux pane. A worker reads the visible screen
using `tmux capture-pane`, parses it with Ghostty's VT engine, and renders owned
styled cells in Ratatui. It does not replay escape sequences to your terminal.
The VT handle stays on the capture worker; no non-Send handles cross threads.

![Synthetic read-only terminal preview](docs/terminal-preview.png)

The preview is opt-in, refreshes about every two seconds, and never sends input,
approves requests, changes tmux buffers, or saves pane contents. It checks the
process and pane identity before and after capture and discards stale results.
Commands have a two-second deadline and 1 MiB output bound; panes are limited to
500×200 cells. It shows a visible-screen snapshot, not continuous PTY output or
full scrollback. Wide previews are clipped to the available dashboard width.

**libghostty-vt does not extract screens from Ghostty.app.** Ghostty-only and SSH
previews are not implemented. Ghostty focus uses AppleScript; tmux navigation
uses the exact socket, pane and TTY. Supported rendering and limitations are
listed in [validation](docs/VALIDATION.md). The [2026-09-16 upstream audit](docs/LIBGHOSTTY.md)
compares the pinned Rust binding, released Ghostty and unreleased APIs.

## SSH, explicit bindings and hooks

Install the same version on each remote host, using your existing SSH config:

```sh
ttybird hosts add lab --binary /home/me/.local/bin/ttybird
ttybird hosts list
ttybird focus EXACT_SESSION_ID --host lab
ttybird hosts remove lab
```

SSH collection uses BatchMode with an eight-second deadline, a 2 MiB output
limit and at most four concurrent hosts. Failure is shown as unavailable.
Remote focus opens an interactive SSH connection; raw TTYs cannot generally
be reattached without a persistent multiplexer.

Manual local binding:

```sh
ttybird terminals --json
ttybird bind EXACT_SESSION_ID --ghostty TERMINAL_UUID
ttybird bind EXACT_SESSION_ID --tmux %3 --socket work
```

`ttybird hooks` prints optional Claude hook configuration for manual integration
into your existing settings. It does not rewrite them. Hooks save allowlisted
lifecycle metadata, not prompts. See `ttybird --help` for CLI options.

## Privacy and development

Collection exports metadata, not prompts, tool arguments or raw command lines.
Explicit terminal and conversation views can contain sensitive text; they remain
in memory and are excluded from logs/JSON. Titles, IDs, directories and host names are personal
metadata: inspect exported snapshots before sharing them.

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and
`cargo test --locked`. [Validation](docs/VALIDATION.md) gives the private tmux/PTY
checks and current platform limits. [The test audit](docs/TEST-AUDIT.md) records
what was removed or consolidated and the practical regressions retained.

MIT licensed. Report reproducible issues with OS, TTYbird/terminal versions and
sanitized evidence; do not attach live transcripts or credentials.
