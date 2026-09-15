# Process, session, and terminal evidence

TTYbird observes three different things: a process, a logical agent session, and a terminal device associated with a process. None alone proves an active model request or an open Ghostty pane.

## Collection rules

1. Enumerate same-user processes using sysinfo, match supported CLI executables/launchers, and exclude zombie/dead statuses. Sleeping or stopped processes still exist; they are not called working merely because they exist.
2. Identify processes by PID and start time. Read `ps` for the current controlling TTY; reject empty/unknown markers, malformed paths, and missing/non-character devices. Headless command modes intentionally suppress inherited TTYs.
3. Associate Codex/Claude logs only through a unique writable descriptor owner. A retained child log proves a hosting process association, not a currently running child. Raw process rows remain when no log association is available.
4. Read bounded log samples. Running-state freshness uses the lifecycle record's own timestamp, not file mtime; unrelated metadata writes must not refresh old work. Missing/stale/omitted lifecycle records stay unknown. Model metadata has its separate bounded lookback.
5. Recheck process identity/status after sampling. A process that exits during collection loses its live row; its log can remain as history with no PID, TTY, or target.
6. Apply saved navigation bindings only to an already observed matching PID/start/session. A saved binding cannot create live status for a historical log. Focus/preview revalidate identity again at use time.
7. On refresh failure, the dashboard removes live PID/TTY/target claims from retained records and reports liveness unavailable. It does not keep presenting an old snapshot as currently live.

The TUI hides log-only history by default. `h` or `--history` shows it explicitly. It also hides hosted children whose activity is idle, ended or unknown; `b` reveals these retained children and raw headless processes. Unknown children may still be doing work that passive collection cannot observe, so this is a presentation filter, not a declaration that they have ended. Roots and children with fresh working/waiting evidence remain visible. JSON snapshots retain the evidence; `--live-only` filters by current process association, including retained sessions on live backends. It does not mean active model calls only.

Collection is a bounded sequence of observations, not an atomic OS snapshot. The final check refreshes process identity/status; TTY and writable-descriptor ownership were observed earlier in that collection. A process that closes its log or changes its TTY during sampling can therefore leave one snapshot with the earlier association until the next refresh. Focus/preview revalidate their required evidence at use time, which still cannot make arbitrary concurrent OS changes atomic.

## What a TTY proves on macOS

The synthetic PTY test closes the master while the fixture process ignores SIGHUP. On this machine macOS keeps the slave TTY assigned to that surviving process, and `ps` continues reporting it. TTYbird correctly agrees with the kernel assignment. This does **not** prove that a GUI pane or terminal connection remains open.

Consequently the UI calls these host TTYs/assignments. Ghostty 1.3.1's installed scripting API exposes pane IDs, titles, and directories, not a per-pane PID/TTY mapping. An explicit binding is a user-selected navigation target; its presence in the saved file is not continuous pane-liveness proof. Ghostty checks that the pane exists when focusing it. tmux preview validates the current process identity, TTY, and pane identity around capture.

The table labels a saved Ghostty target as `Ghostty binding`. Details distinguish that target from kernel TTY evidence. Parent/child PID sharing is recognized by PID and start time even if neither has a TTY; sharing a terminal is a separate observation.

No evidence here establishes a Codex or Ghostty defect. Retaining logs may be intentional, and macOS retaining a controlling TTY for a surviving process is distinct from an active session. The corrected TTYbird defects were overreliance on saved bindings, lack of zombie exclusion, and misleading freshness based on file mtime.

## Reproduction

```sh
python3 scripts/liveness_smoke.py target/release/ttybird
cargo test --release --locked -- --test-threads=2
cargo test --release --locked --test tmux_preview -- --ignored
python3 scripts/tmux_preview_smoke.py target/release/ttybird
```

The liveness script compiles a small synthetic `codex` executable, uses private PTYs/logs/config, and never sends keys to real agents. It verifies live PID/start/TTY against ps, closing the writable log while the process stays alive, stale binding rejection, unreaped zombies, reaped exits, and the PTY-hangup observation. All fixtures are cleaned up. The generic PID-reuse and collection-failure cases also have unit coverage.

Native VT tests use libghostty-vt 0.2.1 directly for cursor movement, carriage-return overwrite, erase, alternate-screen restoration, wide Unicode, reset/style reset, and ignored control sequences. Private tmux tests exercise the real capture path with synthetic contents. These are separate from Ghostty GUI rendering and focus; no GUI-renderer equivalence is claimed.

## Session insights and duplicate process rows

A provider-recorded title and token count describe a session, not its current
liveness. Codex cumulative snapshots replace older snapshots; Claude sampled
message usage is deduplicated and marked partial. Missing counts stay unknown.
Recent plaintext is read only by the explicit local `c` action after identity
revalidation, and is never part of a snapshot or JSON export.

A raw process in the same host/provider/workspace as a top-level live session
is hidden by default as auxiliary. `b` reveals it. This reduces duplicate-looking
rows without claiming the two processes are one session: the collector's
identities and navigation targets remain separate.
