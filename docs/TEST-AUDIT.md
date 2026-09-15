# Test audit

Baseline scope: 85 Rust tests in the collector/UI/CLI/navigation modules and integration tests, plus four smoke scripts. The native VT/capture tests were audited separately and are inventoried below. At that audit, the integrated result was 76 ordinary tests plus the explicit private tmux test. Local checks passed; this is not a claim of lower test runtime.

Decision rule: a test earns its cost only when removing it would let a plausible user-visible, safety, compatibility, or resource-lifecycle regression pass. **Merge** means retain the behavior assertion while deleting the standalone test after moving it into the named neighboring test. **Delete/replace** means the current assertion does not independently protect a useful behavior; a replacement may still be called for.


Audit baseline: the pre-alpha suite. Names below include removed tests so each
deletion remains reviewable; the applied decisions are listed first. No test
count or coverage target was used. Fast policy tests and expensive real-process
checks are kept when they prevent a distinct user-visible failure.

## Applied decisions

| Original test(s) | Outcome and remaining guarantee |
|---|---|
| `main::command_line_contract` | Deleted: Clap's definition self-check; actual CLI behavior remains covered. |
| `navigation::ghostty_focus_passes_id_as_a_separate_argument` | Deleted: checking an AppleScript string was weaker than the real TUI chooser scenario with an exact-ID focus sink. |
| `providers::only_codex_and_claude_have_supported_session_logs` | Deleted: public `providers --json` integration is the single capability contract. |
| `config::round_trip_and_duplicate` | Merged into `host_registration_round_trip_and_no_shell_injection`; duplicate add now proves the original binary is unchanged. |
| `hooks_only_print_allowlisted_configuration`, `failed_hook_does_not_block_agent` | Merged into `hooks_and_hook_failure_are_read_only_and_nonblocking`. |
| `bounded_runner_captures_stdout`, `collect_parses_fake_ssh_snapshot`, `collect_rejects_protocol_mismatch` | Merged into `collect_enforces_snapshot_protocol`; valid output capture and incompatible protocol rejection remain. |
| `empty_process_arguments_never_panic` | Merged into negative provider-classification cases. |
| `resolves_tty_on_an_isolated_tmux_server_when_available` | Removed from default unit tests; exact target-TTY assertion now runs in the explicit private tmux integration. Missing tmux no longer silently passes this coverage. |
| `skips_interpreter_flags_before_the_actual_script` | Kept after review: two short grammar cases are clearer separately than widening every provider-table row. |

The infinite-output scenario now checks both stdout and stderr bounds. The
detached-pipe test requires Python explicitly instead of silently returning
success. Nix/CI supply it. The Ghostty chooser smoke now terminates a synthetic
process while its picker is open and verifies zero binding writes/focus calls.

## Integration gaps identified

1. **Resolved:** the Ghostty picker smoke now kills its selected synthetic process before Enter and verifies zero binding writes or focus calls.
2. Add an end-to-end stale tmux-focus test: replace/recreate the selected pane (or change its TTY) after the displayed snapshot and before Enter; assert ttybird refuses to focus and leaves the active pane unchanged. Existing tmux tests protect preview identity, not the focus path.
3. Strengthen subprocess lifecycle tests to record descendant PIDs and prove they disappear after timeout/stdout/stderr-limit failures. Current tests prove prompt return and direct-child wait, but not process-group descendant cleanup. Also test stderr overflow.
4. Test quitting the dashboard while a synthetic remote collector/pane query is hung. Assert the spawned helper is terminated/reaped within a deadline. The interactive code drops channels but does not retain/join worker handles, and every current TUI smoke uses `--local` or quick stubs.
5. Replace weak/internal assertions before pruning: the Ghostty AppleScript string check is already covered better by `ghostty_picker_smoke.py`; the notice/search test does not perform the transition named by the test; `clap::debug_assert` only checks library wiring.

## Keep

### `src/config.rs`

- `refuses_existing_lock_and_preserves_data` — catches concurrent/interrupted mutation overwriting valid state. Cheap, deterministic, and directly protects user configuration.

### `src/collect.rs`

- `codex_ignores_partial_json_and_extracts_metadata` — catches a partially written JSONL tail preventing all usable metadata/lifecycle recovery. This is a normal live-log state and central to read-only collection.
- `rejects_missing_tty_and_dead_process_statuses` — catches zombies/dead processes appearing focusable and malformed TTY text becoming a navigation target. Keep the status cases; the string cases could be table-driven more tersely.
- `metadata_writes_do_not_refresh_old_lifecycle_events` — catches fresh metadata making an old active turn appear currently working. Alpha-critical stale-state protection.
- `parent_id_comes_only_from_spawn_metadata` — catches unrelated JSON fields manufacturing a parent/child link and causing misleading hierarchy or shared-host claims.
- `codex_model_lookback_recovers_context_before_tail` — catches bounded reading losing a recent model record located between the head and final tail. The internal seam assertions are justified by the bounded-I/O contract.
- `claude_extracts_only_status_metadata` — catches Claude parsing returning message content or losing ID/cwd/model/stop state. It enforces the privacy boundary using synthetic content.
- `shared_open_file_is_not_assigned_arbitrarily` — catches two live processes sharing a descriptor being arbitrarily attached to one session, which could enable wrong focus.
- `descriptor_owner_must_match_log_provider` — catches a Claude-owned descriptor being used as proof for a Codex log (or vice versa).
- `live_descriptor_owned_log_survives_recent_cutoff` — catches a live but old-mtime log being discarded, and simultaneously checks wrong-provider/unowned old logs stay excluded. Keep as a collection-policy test.
- `lsof_parser_accepts_only_writable_descriptors` — catches read-only historical files becoming liveness evidence on non-Linux Unix. Platform-specific but cheap.
- `linux_fdinfo_parser_accepts_only_writable_descriptors` — same contract for Linux access flags; catches read-only descriptors becoming liveness evidence.
- `lifecycle_does_not_cross_an_unread_middle` — catches a start event from the sampled head being combined with unrelated metadata in the tail across an unread gap.
- `claude_tool_use_is_not_idle_and_children_are_distinct` — catches active tool use being shown idle and sidechain children collapsing into their parent. These are separate assertions but arise from the same Claude-record branch.
- `collection_labels_unmatched_logs_as_historical` — catches a log-only session inheriting PID/TTY/activity or observed confidence. This is stronger than testing the `activity` helper alone.

### `src/main.rs`

- `dashboard_defaults_only_for_interactive_human_output` — catches default startup emitting terminal controls into pipes or suppressing the TUI on a real terminal. Pure and cheap.
- `failed_focus_restores_only_its_own_binding` — catches rollback overwriting a concurrent writer or deleting a previous mapping. This is a concrete data-loss/race guard.
- `pane_choice_rejects_changed_session_identity` — catches PID reuse, TTY movement, provider/host/session substitution, or process exit during pane choice. Keep even after adding the missing end-to-end stale-picker scenario.
- `saved_binding_cannot_override_a_resumed_live_session` — catches stale persisted PID/start identity attaching a new process or historical row to an old target.
- `strip_terminal_controls` — catches control bytes from errors/IDs reaching plain-text output. Keep as a direct output-safety contract; extend its cases to C0, DEL, and newline if `clean` remains shared by notices and table output.
- `bind_requires_one_target` — catches an ambiguous or targetless explicit bind command reaching mutation logic. This is an application contract, not a Rust type guarantee.

### `src/model.rs`

- `provider_names_match_serialized_values_and_unknown_is_forward_compatible` — catches drift between the hand-maintained `as_str()` API and serde wire values, and loss of forward-compatible deserialization. Low cost; not a library tautology because two independent mappings are maintained.

### `src/navigation.rs`

- `validates_exact_ghostty_uuid` — catches malformed/untrusted terminal IDs reaching AppleScript and verifies Ghostty has no fabricated TTY. Keep the behavioral validation; the `target_tty(None)` assertion may move to a target-validation table.
- `validates_tmux_pane_and_socket` — catches option injection and non-exact pane names reaching tmux.
- `parses_tmux_environment_from_the_right` — catches socket paths containing commas being truncated, which would query/focus the wrong server.
- `parses_only_exact_tmux_pane_rows` — catches malformed or extra-field tmux output becoming a navigation target.

### `src/providers.rs`

- `classifies_verified_native_executables` — catches a supported CLI disappearing from process discovery. The table is the concise form of this compatibility contract.
- `classifies_verified_interpreter_package_scripts` — catches real npm/Python/Bun launch shapes being missed. Keep provider fixtures as documented path shapes rather than copying classifier predicates.
- `resolves_one_package_manager_script_symlink` — catches package-manager shims being classified as generic interpreters. Filesystem behavior is real and not covered by string-only cases.
- `rejects_generic_interpreters_prompts_and_shell_mentions` — catches prompt text, inline code, shell strings, or lookalike packages being mistaken for live agents. High-value false-positive protection.
- `generic_names_require_provider_specific_evidence` — catches ubiquitous `agent`, `pi`, or `goose` processes being claimed without executable-path/subcommand evidence.
- `headless_detection_uses_cli_positions_only` — catches prompt text being interpreted as flags and headless/background agents polluting the interactive list. The cases are numerous but represent materially different CLI grammars.

### `src/remote.rs`

- `validates_remote_identifiers` — catches SSH option/shell injection through host and remote binary configuration.
- `bounded_runner_stops_at_stdout_limit` — catches unbounded memory capture and proves the error reason. Strengthen it to record/reap descendants.
- `bounded_runner_stops_infinite_output_without_channel_deadlock` — catches the reader threads/channel deadlocking once output exceeds the cap; distinct from a finite over-limit payload.
- `bounded_runner_kills_and_reaps_on_timeout` — catches a hung SSH/osascript blocking forever. Strengthen the fixture to expose direct and descendant PIDs and verify disappearance, not only elapsed time.
- `bounded_runner_does_not_wait_for_detached_pipe_holder` — catches a detached descendant holding inherited pipes and preventing return after a failure. It has higher platform/runtime cost; keep in an explicit Unix integration lane if default-suite flakiness appears.

### `src/telemetry.rs`

- `resumed_hook_never_inherits_another_process_terminal` — catches a resumed session inheriting a stale process's TTY/target while preserving mappings for the exact PID/start identity.
- `only_explicit_permission_notification_waits_for_input` — catches generic notifications or subagent-stop events being presented as user action required.
- `rejects_path_traversal` — catches hook session IDs escaping the private event directory.
- `oversized_hook_is_not_written` — catches unbounded/untrusted hook payload persistence and proves rejection leaves no event directory.

### `src/ui.rs`

- `renders_wide_medium_and_narrow_without_leaking_controls` — catches untrusted workspace text writing escape controls and gross responsive-layout regressions. Reduce to boundary widths only if runtime matters; ratatui buffer rendering is deterministic.
- `parent_and_subagent_keep_their_own_roles_and_models` — catches child model/role data being replaced by parent metadata or vice versa; details also protect exact shared-host identity messaging.
- `tree_rows_are_depth_first_and_disambiguate_nested_sessions` — catches non-contiguous hierarchy, unstable depth-first order, and ambiguous repeated workspace labels.
- `branch_controls_persist_across_refresh_and_navigate_relations` — catches collapse state/selection loss on refresh and broken parent/child arrow navigation.
- `search_reveals_collapsed_child_and_restore_selects_visible_ancestor` — catches search hiding a match under a collapsed branch or leaving selection on an invisible row after clearing.
- `linked_subagent_does_not_claim_shared_host_without_exact_identity` — catches a linked child with a different PID/start/TTY falsely claiming the parent's host process and terminal.
- `hierarchy_cycles_terminate_and_parent_lookup_stays_on_host` — catches infinite traversal and cross-host parent metadata leakage. Both protect hostile/corrupt snapshot input.
- `failed_refresh_does_not_keep_old_live_tty_claims` — catches stale PID/TTY/preview/picker state remaining focusable after collection fails.
- `history_is_opt_in_and_never_counts_as_live_tty` — catches history appearing live by default or inflating live PID/TTY counts.
- `selection_survives_reordered_snapshots_by_identity` — catches periodic refresh moving Enter to a different session because selection was retained by row index.
- `hides_only_raw_headless_rows_and_attention_is_strictly_observed` — catches legitimate subagents being hidden with raw headless processes and inferred attention being treated as observed.
- `empty_and_filtered_views_explain_the_state` — catches blank dashboard states with no explanation. This is user-facing alpha behavior; keep assertions on semantic phrases rather than full layout.
- `preview_replaces_details_scrolls_and_crops_on_narrow_terminals` — catches sensitive/full-width terminal content escaping the preview viewport and details being rendered underneath it.
- `preview_preserves_passed_terminal_styles` — catches the TUI flattening the already-sanitized terminal renderer's colors/modifiers; retain only if style fidelity is a product requirement.
- `ghostty_picker_renders_frozen_session_candidates_and_selection` — catches the picker omitting frozen process identity, rendering control characters, or highlighting the wrong pane before confirmation.

### `tests/cli.rs`

- `collector_protocol_and_read_only_state` — catches the public collect command changing protocol shape, persisting config, or leaking prohibited prompt/command fields.
- `explicit_dashboard_refuses_pipes_without_terminal_escape_sequences` — catches explicit TUI mode contaminating non-TTY output and verifies the failure channel remains stderr.
- `provider_catalog_distinguishes_process_support_from_log_support` — catches misleading public capability claims. It is the authoritative end-to-end replacement for the duplicate provider unit test listed under Delete.

### `tests/tmux_preview.rs`

- `live_capture_preserves_pane_and_buffers_and_rejects_wrong_identity` — catches terminal capture mutating tmux pane state/buffers, mishandling VT/Unicode, or bypassing host/PID/start/TTY identity. Keep ignored/explicit because it requires tmux and sleeps; make missing tmux an explicit skip in the invoking harness rather than a false pass.

### Smoke scenarios

- `scripts/ghostty_picker_smoke.py` (`main`) — strongest wrong-focus coverage today: proves no implicit cwd binding, cancel is side-effect free, and the explicitly selected second UUID alone is saved/focused. Keep in release/local smoke; add the stale-picker branch described above.
- `scripts/liveness_smoke.py` (`main`) — uniquely exercises real OS PID/start/TTY, descriptor closure, zombie exclusion, reap, and PTY hangup with a synthetic binary. Keep despite compiler/process cost; it catches failures unit fixtures cannot.
- `scripts/tmux_preview_smoke.py` (top-level scenario) — proves the installed TUI opens/closes/reopens a real private-tmux preview and restores terminal attributes. Keep as an explicit smoke, sharing PTY helpers with `tui_smoke.py` if maintenance becomes costly.
- `scripts/tui_smoke.py` — checks restoration for normal quit, Ctrl-C, SIGTERM and a truncated input sequence followed by SIGTERM. Dashboard PTY-hangup cases exercise both initially ignored/default SIGHUP and closure before/during timed polling. These catch the observed detached-dashboard CPU loop; hanging up a fake agent in `liveness_smoke.py` did not cover it. Restoration compares configurable termios and mutable status flags, not kernel bookkeeping bits. Cleanup owns the fixture process group until its final signal and reaps the leader.

## Merge, then delete the standalone test

### Configuration and CLI

- `src/config.rs::round_trip_and_duplicate` + `tests/cli.rs::host_registration_round_trip_and_no_shell_injection` — extend the CLI test to attempt a duplicate `lab` add and confirm the original binary remains, then remove the unit round trip. The integration already proves read/write/list/remove and unsafe-name rejection; one added assertion preserves duplicate protection. Saves one tempdir and avoids testing `add_host`/serialization exactly as the CLI composes them.
- `tests/cli.rs::hooks_only_print_allowlisted_configuration` + `tests/cli.rs::failed_hook_does_not_block_agent` — one `hooks_and_hook_failure_are_read_only_and_nonblocking` integration can invoke both subcommands in the same tempdir. Preserve JSON allowlist shape, empty stdout/success for empty input, and absence of config/events. Modest runtime saving (one setup; still two process launches), clearer privacy boundary.

### Collector sampling and lifecycle

- `codex_completion_is_not_running` into `metadata_writes_do_not_refresh_old_lifecycle_events` — table-drive started/completed/stale/untimed cases around `activity`. Removing it without the merge could miss a completed turn remaining “working.”
- `stale_live_lifecycle_is_unknown` into the same lifecycle table — its two assertions duplicate the helper outcome already exercised by stale metadata; retain both Started and Completed stale expectations in the table, then delete the standalone test.
- `long_middle_line_does_not_force_a_full_file_read` + `bounded_range_keeps_a_line_starting_exactly_at_the_seam` — one bounded-reader test can cover oversized unread middle, exact seam inclusion, fragment exclusion, head metadata, and tail completion. This preserves the real bugs while consolidating low-level file construction.
- `codex_model_older_than_lookback_is_unknown` + `codex_model_lookback_ignores_unterminated_record` into `codex_model_lookback_recovers_context_before_tail` — share one fixture builder and table-drive: recover within lookback, return unknown beyond lookback, ignore an unterminated last record. Saves several multi-megabyte file writes while preserving all three boundary bugs.

### Navigation/provider contracts

- `resolves_tty_on_an_isolated_tmux_server_when_available` into the explicit tmux integration suite (`tests/tmux_preview.rs` or a small `tests/tmux_navigation.rs`) — the current default test silently returns success when tmux is absent and creates a real server when present. An ignored integration should require tmux, verify query/parse/target-TTY round trip, and share the private-server cleanup fixture.
- `skips_interpreter_flags_before_the_actual_script` into `classifies_verified_interpreter_package_scripts` — add Node-with-value-flag, Python-short-flag, and instrumented Gemini rows to the interpreter table. Removing them outright could miss wrappers being lost, but a separate test adds no isolation.
- `empty_process_arguments_never_panic` into `rejects_generic_interpreters_prompts_and_shell_mentions` — add empty argv rows for Node/Python/Bun. The panic guard is real, but it is simply another negative classifier input.

### Remote runner/protocol

- `bounded_runner_captures_stdout` into `collect_parses_fake_ssh_snapshot` — the latter already succeeds only if stdout is captured and delivered to JSON parsing. Retain an exact parsed host/protocol assertion; the basic byte echo tests subprocess plumbing rather than a separate product behavior.
- `collect_rejects_protocol_mismatch` into `collect_parses_fake_ssh_snapshot` — table-drive protocol 1 success and protocol 99 rejection through the same fake SSH fixture. Saves process setup without losing compatibility protection.

### UI state/rendering

- `live_only_keeps_branch_collapse_effective` into `branch_controls_persist_across_refresh_and_navigate_relations` — rerun the same parent/child collapse assertion with `live_only = true`. The standalone fixture is a strict subset of the branch behavior.
- `role_does_not_infer_main_from_missing_parent_metadata` into `parent_and_subagent_keep_their_own_roles_and_models` and `history_is_opt_in_and_never_counts_as_live_tty` — add ordinary/raw/history label assertions to those role/history fixtures. This retains false-role protection while avoiding a third synthetic session setup.
- `full_details_exposes_paths_and_scrolls_on_small_terminals` into the overlay rendering group headed by `preview_replaces_details_scrolls_and_crops_on_narrow_terminals` — preserve the cwd/source visibility assertion, but replace the weak “Details” assertion after setting scroll with evidence that the viewport actually changes or clamps. Shared overlay fixtures reduce rendering calls and improve signal.
- `preview_explains_when_capture_is_unavailable` into `preview_replaces_details_scrolls_and_crops_on_narrow_terminals` — table-drive populated and unavailable preview states at narrow width. Preserve the actionable unavailable message and close hint.
- `empty_ghostty_picker_remains_clear_on_narrow_terminal` into `ghostty_picker_renders_frozen_session_candidates_and_selection` — render populated and empty picker states from a shared fixture, retaining narrow-width empty/cancel wording.

## Delete or replace

- `src/main.rs::command_line_contract` — `Cli::command().debug_assert()` asks clap to validate its own generated command graph and asserts no ttybird-visible behavior. Compile/derive checks already cover types, while `bind_requires_one_target` and CLI integrations cover actual contracts. Deleting it would only miss an internal clap configuration panic; replace with a concrete help/argument behavior only if one has regressed before.
- `src/navigation.rs::ghostty_focus_passes_id_as_a_separate_argument` — coupled to string contents of an AppleScript constant and does not execute command construction. `scripts/ghostty_picker_smoke.py` already proves the selected UUID reaches fake `osascript` exactly once as an argument. Delete this unit after keeping that smoke; if fast coverage is needed, extract a pure argv builder and assert its vector.
- `src/providers.rs::only_codex_and_claude_have_supported_session_logs` — exact policy is exercised through the public `providers --json` integration for every provider. Keeping both makes each new provider require two synchronized policy lists without catching an additional bug.
- `src/ui.rs::navigation_notice_remains_visible_after_search_confirmation` — the test never confirms search: it independently renders a notice, sets `searching = true`, then only checks that “Search” appears. It would pass if Enter cleared the notice, so it catches no bug named by the test. Replace with a reducer/event-level transition test or extend `scripts/tui_smoke.py` to induce a navigation error, open search, press Enter, and assert the notice remains.

## Missing alpha-critical coverage

### Wrong focus

- **Resolved:** stale Ghostty picker confirmation now exercises the real event path and proves zero binding/focus side effects.
- **High:** no test performs Enter-to-focus after a tmux pane has been replaced or moved. Assert current PID/start and pane TTY are re-read and the active pane stays unchanged on mismatch.
- **Medium:** no end-to-end test selects a row, refreshes into a different ordering/duplicate ID on another host, then presses Enter. The identity-based unit is good; a fake focus sink would prove event routing uses `(host, provider, id)` rather than visible index/ID alone.
- **Medium:** remote focus has no argv/quoting contract test. Use a fake SSH executable or extracted argv builder to assert the host is after `--`, the ID cannot add shell syntax, and failed SSH never reports success.

### Stale process and state

- **High:** `failed_refresh_does_not_keep_old_live_tty_claims` calls `invalidate_liveness` directly. There is no interactive test where a collector failure arrives while a pane query/preview is in flight; assert the pending query/result cannot restore a focusable stale target.
- **Medium:** add a binding file containing duplicate entries for the same session with old and current PID/start values; assert only the exact current identity can win and result ordering cannot resurrect the stale target.
- **Medium:** liveness smoke covers close/zombie/reap but not PID reuse in the collector itself. A narrow injectable identity-source test can model the same PID with a changed start time without relying on OS PID reuse.

### Subprocess cleanup

- **High:** timeout and output-limit tests do not prove non-detached descendants in the created process group are gone. Have a fixture write child/grandchild PIDs, fail the runner, poll `kill(pid, 0)`/`waitpid`, and clean up defensively.
- **High:** dashboard worker threads and their SSH/osascript children are not exercised during quit. A smoke with a hanging fake remote command should prove q/Ctrl-C/SIGTERM leave no helper behind.
- **Medium:** no stderr-limit test exists even though stderr has a separate cap and reader path. Generate infinite stderr, expect the private size error (without echoed content), and prove the tree is reaped.
- **Medium:** `bounded_runner_does_not_wait_for_detached_pipe_holder` proves prompt failure return but manually kills the detached fixture. Keep that cleanup, and add a successful-parent case to define whether inherited pipes from a detached descendant should be ignored or treated as timeout.

## Cost and flakiness tradeoffs

- Fast deterministic unit tests should retain policy and safety edges, not duplicate library wiring. The proposed unit merges mainly remove repeated multi-megabyte file creation, tempdirs, and ratatui renders while preserving assertions.
- Real-process tests provide evidence unavailable from mocks, but belong in explicit smoke/integration lanes: tmux availability, C compiler/Node/Python presence, PTY timing, macOS `/var` canonicalization, and 50 ms deadlines can vary under load. A missing dependency should be reported as a skip by the harness, never silently pass inside a test.
- `remote` timing assertions should use generous outer deadlines plus explicit PID cleanup evidence. Elapsed-time-only checks are prone to load flakes and can pass while descendants leak.
- The four Python smokes repeat PTY drain/wait/cleanup code. Extract a small `scripts/smoke_support.py` helper only if these scenarios continue growing; keep scenario assertions separate so a failure still identifies focus, liveness, preview, or screen restoration.
- Do not collapse all collector cases into one giant test. Merge only cases sharing the same fixture/branch; descriptor ownership, provider isolation, and historical classification should remain separate because they diagnose distinct safety failures.

## Applied collector and UI consolidation

## Exact old-to-new mappings

### `src/collect.rs`

- `metadata_writes_do_not_refresh_old_lifecycle_events`
- `codex_completion_is_not_running`
- `stale_live_lifecycle_is_unknown`
  -> `codex_lifecycle_requires_fresh_events_and_completion_is_idle`
  - Preserved old lifecycle timestamp, untimed lifecycle, fresh start, fresh completion, stale start, and stale completion assertions. One test now describes the single `activity`/Codex lifecycle contract.

- `long_middle_line_does_not_force_a_full_file_read`
- `bounded_range_keeps_a_line_starting_exactly_at_the_seam`
  -> `bounded_reader_handles_a_long_gap_and_exact_seams`
  - Preserved bounded head/tail parsing, completion after a long unread line, exact seam inclusion, and partial-fragment exclusion. Both cases exercise the bounded reader and share one tempdir.

- `codex_model_lookback_recovers_context_before_tail`
- `codex_model_older_than_lookback_is_unknown`
- `codex_model_lookback_ignores_unterminated_record`
  -> `codex_model_lookback_respects_boundaries_and_partial_records`
  - Preserved all internal sample assertions and all three externally parsed outcomes: recovery inside the lookback, unknown beyond it, and rejection of an unterminated final record. The files share a tempdir. This is a maintenance consolidation; it does not claim lower wall time because Rust previously could run the standalone tests in parallel.

Result: 20 collector tests became 15; no unique assertion was removed.

### `src/ui.rs`

- `branch_controls_persist_across_refresh_and_navigate_relations`
- `live_only_keeps_branch_collapse_effective`
  -> `branch_controls_persist_across_refresh_and_navigate_relations`
  - Added the `live_only` collapse/selected-parent assertions to the existing branch-control contract.

- `role_does_not_infer_main_from_missing_parent_metadata`
  -> assertions distributed into:
  - `parent_and_subagent_keep_their_own_roles_and_models` for ordinary-session and raw-process roles/model fallback.
  - `history_is_opt_in_and_never_counts_as_live_tty` for the history role after history is explicitly shown.
  - Preserved all three former role outcomes while placing each beside its owning behavior.

- `full_details_exposes_paths_and_scrolls_on_small_terminals`
- `preview_replaces_details_scrolls_and_crops_on_narrow_terminals`
- `preview_explains_when_capture_is_unavailable`
  -> `details_and_preview_overlays_render_and_crop_on_narrow_terminals`
  - Preserved detail cwd/source/small-terminal assertions, preview scroll/crop/overlay precedence assertions, and unavailable-preview guidance.

- `ghostty_picker_renders_frozen_session_candidates_and_selection`
- `empty_ghostty_picker_remains_clear_on_narrow_terminal`
  -> `ghostty_picker_renders_frozen_session_candidates_and_selection`
  - Preserved populated picker identity, sanitization, selection-style and instruction assertions plus empty/narrow picker guidance.

- `navigation_notice_remains_visible_after_search_confirmation`
  -> removed without replacement.
  - It never performed search confirmation or any event transition. It only rendered a notice, then set `searching = true` and asserted the unrelated `Search` label. It would pass even if Enter cleared the notice, so its two assertions provided no protection for the named behavior.

Result: 21 UI tests became 15. All unique behavior assertions were preserved except the two non-probative notice/search assertions.


## Native VT and capture-boundary inventory

## Test audit

### `src/preview.rs`

- `preserves_rows_from_tmux_lf_capture_without_scrolling` — kept. Catches the real tmux-LF versus terminal-LF mismatch and final-delimiter scroll bug.
- `lf_after_full_width_row_does_not_trigger_pending_wrap` — kept. Catches a full-width pending-wrap row inserting/scrolling an extra line.
- `renders_unicode_wide_and_combining_graphemes_once` — kept. Catches duplicated wide tails and lost combining codepoints.
- `converts_sgr_colors_and_modifiers` — kept. Catches loss of explicit RGB foreground/background, bold, or italic styling.
- `interprets_cursor_controls_and_drops_osc_and_queries` — removed as a duplicate: cursor/erase behavior was already exercised by the next test, while side-effect controls were already exercised by the control-string test.
- `applies_carriage_return_cursor_motion_and_erases` -> `applies_cursor_motion_and_erasure` — renamed and kept. Catches CR overwrite, cursor movement, line erase, screen erase, and cursor repositioning.
- `switches_and_restores_the_alternate_screen` — kept. Catches rendering the wrong primary/alternate buffer after DEC private mode changes.
- `overwriting_a_wide_character_tail_clears_the_grapheme` — kept. Catches a stale wide glyph when its spacer tail is overwritten.
- `terminal_reset_clears_cells_and_style` — kept. Catches stale cells/styles after RIS.
- `ignores_control_strings_and_unknown_modes` -> `drops_side_effect_control_strings_queries_and_unknown_modes` — renamed and expanded with DSR plus explicit escape/payload absence assertions. Catches OSC, BEL, DCS, APC, query, and unknown-mode leakage without duplicating cursor/erase behavior.
- `rejects_invalid_dimensions_and_oversized_input` — kept. Catches zero/over-cap dimensions and parser input above 1 MiB.

Result: one overlapping parser test was removed while retaining every distinct failure mode.

### `src/terminal_preview.rs`

- `requires_exact_live_pane_and_bounded_size` -> `parses_one_exact_live_pane_record_and_rejects_invalid_state` — renamed and expanded. It now also proves CRLF compatibility, the exact maximum accepted size, missing terminator rejection, duplicate-record rejection, and trailing-whitespace rejection, in addition to pane/TTY/dead/dimension mismatches.
- `requires_a_complete_visible_screen_row_set` — new. Catches zero, missing, unterminated, or excess capture rows that would otherwise be padded/truncated into a misleading successful preview.
- `ghostty_is_not_silently_treated_as_tmux` — kept. Catches dispatching a Ghostty navigation target into tmux capture.

Result: 2 boundary tests became 3 because complete screen framing was previously untested.


## Documents and scripts

| Files | Decision |
|---|---|
| `docs/MARKETING-ALPHA.md`, `docs/NAME.md`, `docs/research/*`, `docs/brand.svg` | Removed: launch plans, channel strategy, naming and research diaries are outside the technical product. |
| Nine historical `docs/*smoke-summary.json` files | Removed: repeated dated snapshots, some contradictory or tied to developer-machine paths. Commands and bounded claims now live in VALIDATION; CI provides commit-specific results. |
| `docs/liveness-preview.png` | Removed: duplicate dashboard image. |
| README (EN/JA) | Rewritten around installation, finding/returning, capabilities and limits. |
| ARCHITECTURE / LIVENESS / PROVIDERS / NIX | Kept: distinct implementation contract, identity model, adapter evidence and build requirements. Removed speculative extension order and machine-inventory anecdotes. |
| VALIDATION | Consolidated: runnable checks and proof boundaries, no repeated per-version history. |
| `scripts/capture_demo.py`, `examples/tui_preview.rs` | Kept: reproducible synthetic media and privacy assertion; not counted as live integration evidence. |
| `scripts/package_release.py` | New release check: rejects developer-specific dynamic libraries before packaging tested binaries and licenses. |
| `scripts/nix-local.sh` | Kept as an optional untracked-source development helper. Normal clones use the flake directly. |
| `scripts/ghostty_focus_smoke.py` | New opt-in real GUI test; two owned windows, actual CLI focus and dead-process rejection, cleanup and original-focus restoration. One initial unexplained failure and one pass are documented in VALIDATION; it is not counted as stable CI coverage. |
| Four PTY/process smoke scripts | Kept as separate scenarios: each checks a different integration failure. Shared drain/cleanup scaffolding could be extracted if it grows; abstraction now would add indirection without removing distinct scenarios. |

Consolidation reduces repeated fixtures and maintenance; no execution-time
speedup is claimed without measurements. Unresolved integration gaps above are
not implied to be covered by unit tests or parser-library guarantees.

## Session-insights regression boundaries (2026-09-16)

- `collect`: cumulative Codex snapshots must not be summed; a missing tail total
  after a sample gap must not reuse an obsolete head total. Optional zero and
  missing fields remain different. Claude streaming updates must not count the
  same message twice or claim a complete session total.
- `collect`: bounded conversation extraction accepts only user/assistant text;
  identity checks refuse an unidentified Claude sidechain. The recording exercises
  the real local PID/start-time/writable-log read path with synthetic text.
- `model`: old remote snapshots still deserialize; collection-only transcript
  paths must not enter JSON.
- `ui`: hiding auxiliary process rows must not copy their navigation targets;
  titles stay searchable, sampled usage is labeled, and invalidation clears text.
- `preview`: oversized APC input must not hide subsequent visible text. This
  extends the existing side-effect-control regression rather than duplicating it.

The public recording is also a bounded end-to-end scenario: discovery, tree
actions, explicit conversation, native VT preview, and exact private-tmux focus.
Its isolated inventory and text checks prevent real-session data from leaking
into public media. It does not measure model performance or prove GUI focus.
