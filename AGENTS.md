# Project rules

Exception for user-requested handoff: an explicit handoff action may prepare a bounded draft from Git metadata, user-selected notes and optionally a revalidated plaintext conversation excerpt. Show the exact draft and destination before starting a new owned Codex session. Only a confirmed handoff may persist that content privately for the destination to read. Never copy credentials, hidden reasoning, tool arguments or source-provider permission settings. Tests launch synthetic destination programs, not models.

Workspace inspection is read-only Git metadata. Attention notifications are opt-in, use observed lifecycle evidence, and persist only notification metadata/read/snooze state. A completed response is not proof that the task or review is complete.

Preserve active agent sessions. Collection is read-only and must never send terminal input or approve agent actions. Focus/attach require an explicit CLI command. Never read credentials or print prompts, tool arguments, environment variables, or full command lines.

Exception for user-requested managed terminals: `ttybird run -- COMMAND` explicitly launches a new, TTYbird-owned PTY. Its bounded screen state may be kept in daemon memory across dashboard detach, but never persisted or included in collection JSON. Only explicit input mode may forward keys/paste to that owned session; `stop ID` may terminate that owned process group. Existing discovered terminals must never receive input through this feature. Tests use synthetic commands only.

Exception for the user-requested terminal preview: explicit `p` in the TUI may display the selected, verified local tmux pane's visible contents in memory. Never persist, log, export in JSON, or collect these contents in the background when preview is closed. Preview never sends input or changes agent state. Keep synthetic fixtures for preview tests.

Exception for the user-requested conversation view: explicit `c` may read the selected local Codex/Claude transcript after revalidating PID, start time, session ID and unique writable ownership. Display only a bounded recent user/assistant plaintext excerpt in memory; never tool arguments, system messages or reasoning. Close/selection changes clear it. Never export excerpts in JSON, persist them or fetch them with the view closed. Provider-recorded titles and token metadata may be collected; treat titles as personal metadata.

Keep unknown, inferred, and observed states distinct. A live process is not proof of an active model turn. A log is not proof of a live process. Match process identity with PID and start time; never focus based solely on working directory.

Validate with cargo fmt --check, cargo clippy --all-targets -- -D warnings, cargo test, and a local read-only smoke test. Keep fixture logs synthetic. Network and terminal integration tests must state which paths were exercised.
