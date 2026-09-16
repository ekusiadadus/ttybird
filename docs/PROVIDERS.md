# Provider coverage — 0.4.0

TTYbird discovers same-user coding CLI processes without launching or connecting to them. `ttybird providers` prints the current capability matrix; add `--json` for machine-readable output.

Process support and activity support are separate. For every provider below, an exact live process can supply PID, process start time, working directory, and TTY when the operating system exposes them. Codex and Claude Code also have bounded provider-specific metadata adapters. Every other provider added in 0.4.0 remains `unknown` / `Unverified`: TTYbird does not infer working, waiting, or completion from process existence, terminal contents, or generic files.

Explicit Ghostty and tmux bindings work from the verified process identity regardless of provider. Read-only preview remains limited to a selected local tmux pane. It does not turn terminal contents into activity evidence.

## Recognized launchers

The sources below are the official documentation or upstream package manifests checked on 2026-09-15. Native executables are matched by exact basename. Interpreter-hosted CLIs are accepted only when a documented launcher or package-specific script path is present; a generic `node`, `bun`, or `python` process is insufficient.

| Provider | Official command and distribution evidence | Recognized identity |
|---|---|---|
| Codex | [`codex`, OpenAI upstream](https://github.com/openai/codex) | Existing exact `codex` native executable and documented platform binary names |
| Claude Code | [`claude`, Anthropic documentation](https://code.claude.com/docs/en/overview) | Existing exact `claude` / `claude-code` executable identity |
| Gemini CLI | [`@google/gemini-cli`](https://github.com/google-gemini/gemini-cli/blob/main/packages/cli/package.json) exposes `gemini` as `dist/index.js` | Exact `gemini` executable, or package-specific `dist/index.js` and bundled `bundle/gemini.js` interpreter paths |
| OpenCode | [Installation uses `opencode-ai`; command `opencode`](https://github.com/anomalyco/opencode); [source launcher](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/package.json) is `bin/opencode` | Exact `opencode` executable, plus the documented package launcher path |
| Amp | [Amp CLI installation](https://ampcode.com/docs/cli) runs `amp`; [current npm package](https://ampcode.com/news/npm-package-changes) is `@ampcode/cli` and contains a compiled executable | Exact `amp` executable and current package path |
| Cursor CLI | [Cursor documents `cursor-agent`](https://docs.cursor.com/en/cli/installation); the current [official installer](https://cursor.com/install) links both `agent` and `cursor-agent` to its versioned native executable | Exact `cursor-agent`; generic `agent` is accepted only from Cursor's versioned install path |
| GitHub Copilot CLI | [GitHub installs `@github/copilot`](https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/install-copilot-cli) and runs `copilot` | Exact `copilot` executable or `@github/copilot` package launcher; the older `gh copilot` extension is outside this adapter |
| Aider | [`aider-chat`](https://github.com/Aider-AI/aider/blob/main/pyproject.toml) exposes `aider = aider.main:main` | Exact `aider` launcher and package-specific Python entry path |
| Goose | [AAIF Goose `goose-cli`](https://github.com/aaif-goose/goose/blob/main/crates/goose-cli/Cargo.toml) builds the `goose` binary; official examples start an interactive [`goose session`](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/cli-providers.md#usage-examples) | `goose session` from selected upstream and common CLI install paths; `goosed` and other service binaries are outside the interactive CLI adapter |
| Cline CLI | [Cline distribution](https://github.com/cline/cline/blob/main/apps/cli/DISTRIBUTION.md) uses a `cline` wrapper and `@cline/cli-<platform>-<arch>/bin/cline` compiled binaries | Exact `cline` executable and documented wrapper/platform package paths |
| Qwen Code | [`@qwen-code/qwen-code`](https://github.com/QwenLM/qwen-code/blob/main/packages/cli/package.json) exposes `qwen` as `dist/index.js` | Exact `qwen` executable, or the package-specific `@qwen-code/qwen-code/.../dist/index.js` interpreter script |
| Kilo Code CLI | [`@kilocode/cli`](https://github.com/Kilo-Org/kilocode/blob/main/packages/opencode/package.json) exposes `kilo` and `kilocode` through `bin/kilo`; its [resolver](https://github.com/Kilo-Org/kilocode/blob/main/packages/opencode/bin/kilo) launches a platform binary | Exact `kilo` / `kilocode` executable and documented wrapper/platform package paths |
| Factory Droid | [Factory's CLI reference](https://docs.factory.ai/droid-cli/cli-reference) installs the `droid` npm package and runs `droid` | Exact `droid` executable and documented package launcher |
| Crush | [Charm's upstream](https://github.com/charmbracelet/crush) distributes the `crush` native executable, including through `@charmland/crush` | Exact `crush` executable |
| Pi | [Current Pi package](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/package.json) is `@earendil-works/pi-coding-agent` and exposes `pi` as `dist/bundle/cli.js` | `pi` from package-specific install paths, including the current package script; the legacy `@mariozechner/pi-coding-agent` path is retained for installed older releases |
| Mistral Vibe | [`mistral-vibe`](https://github.com/mistralai/mistral-vibe/blob/main/pyproject.toml) exposes `vibe = vibe.cli.entrypoint:main` | Exact `vibe` launcher and package-specific Python entry path; `vibe-acp` and `vibe-app-server` are separate surfaces |

These identities cover the cited distribution forms. Package layouts, compiled wrapper names, and install methods can change upstream. TTYbird deliberately avoids broad matches such as `agent`, `ai`, `node`, `python`, `bun`, `goose`, or `pi` appearing merely as arbitrary arguments or unrelated path components. A missing row can therefore mean an unrecognized install form rather than an absent tool.

## Capability boundary

| Capability | Codex | Claude Code | Other providers above |
|---|---|---|---|
| Same-user process, PID/start identity, cwd, TTY | Yes | Yes | Yes |
| Provider session/log metadata | Bounded rollout sampling | Bounded transcript sampling | No |
| Observed lifecycle source | Existing local control server: initial read + passive status subscription in the dashboard; bounded reads in one-shot collection | Optional allowlisted hooks | No |
| Activity claim | Observed/inferred only when existing evidence supports it; otherwise unknown | Observed/inferred only when existing evidence supports it; otherwise unknown | Always unknown |
| Explicit Ghostty/tmux navigation | Yes | Yes | Yes, when bound to the exact live identity |
| Local tmux visible-screen preview | Yes | Yes | Yes, when the exact process TTY matches the pane |

These process-only adapters do not inspect provider conversation logs or authentication stores. Exports exclude prompts, tool arguments, environment variables and raw command lines. It does not use the terminal preview to classify activity. Adding process support does not imply an official lifecycle integration or endorsement by a provider.

## Verification status

Classifier tests cover the documented launcher shapes with synthetic arguments.
Private PTY/tmux fixtures exercise discovery and preview for a compiled
Codex-shaped process and a Node process using Gemini's documented package path.
These tests do not authenticate with a provider or claim real-install coverage
for every CLI or package manager. Upstream launcher changes can cause missed
processes; report the sanitized executable/package path, not full arguments.
