# Project rules

Preserve active agent sessions. Collection is read-only and must never send terminal input or approve agent actions. Focus/attach require an explicit CLI command. Never read credentials or print prompts, tool arguments, environment variables, or full command lines.

Exception for the user-requested terminal preview: explicit `p` in the TUI may display the selected, verified local tmux pane's visible contents in memory. Never persist, log, export in JSON, or collect these contents in the background when preview is closed. Preview never sends input or changes agent state. Keep synthetic fixtures for preview tests.

Keep unknown, inferred, and observed states distinct. A live process is not proof of an active model turn. A log is not proof of a live process. Match process identity with PID and start time; never focus based solely on working directory.

Validate with cargo fmt --check, cargo clippy --all-targets -- -D warnings, cargo test, and a local read-only smoke test. Keep fixture logs synthetic. Network and terminal integration tests must state which paths were exercised.
