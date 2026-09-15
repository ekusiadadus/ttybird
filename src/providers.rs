use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::model::Provider;

/// Classify an agent process from OS process metadata without inspecting
/// prompts, environment variables, or process output.
pub fn classify(name: &OsStr, executable: Option<&Path>, command: &[OsString]) -> Option<Provider> {
    if let Some(path) = executable {
        let basename = path.file_name().and_then(OsStr::to_str)?;
        if let Some(provider) = classify_native(basename, Some(path), command) {
            return Some(provider);
        }
        if let Some(kind) = interpreter(basename) {
            return interpreter_script(kind, command).and_then(classify_script_path);
        }
    }

    let basename = name.to_str()?;
    classify_native(basename, executable, command).or_else(|| {
        interpreter(basename)
            .and_then(|kind| interpreter_script(kind, command))
            .and_then(classify_script_path)
    })
}

pub const fn has_session_logs(provider: Provider) -> bool {
    matches!(provider, Provider::Codex | Provider::Claude)
}

/// Whether this invocation is a verified non-interactive/server mode.
pub fn is_headless(provider: Provider, command: &[OsString]) -> bool {
    let Some(args) = provider_arguments(provider, command) else {
        return false;
    };

    match provider {
        Provider::Codex => codex_subcommand(args)
            .is_some_and(|value| matches!(value, "app-server" | "mcp-server" | "exec")),
        Provider::Claude | Provider::Gemini | Provider::Cursor | Provider::QwenCode => {
            option_before_positional(args, &["-p", "--print", "--prompt"])
        }
        Provider::OpenCode | Provider::Kilo => {
            first_positional(args).is_some_and(|value| value == "run")
        }
        Provider::Amp => option_before_positional(args, &["--no-tui"]),
        _ => false,
    }
}

fn provider_arguments(provider: Provider, command: &[OsString]) -> Option<&[OsString]> {
    let args = command_after_executable(command)?;
    let runtime = command
        .first()
        .and_then(|value| Path::new(value).file_name())?;
    let Some(kind) = runtime.to_str().and_then(interpreter) else {
        return Some(args);
    };
    let script = interpreter_script(kind, command)?;
    if classify_script_path(script) != Some(provider) {
        return None;
    }
    let script_index = args
        .iter()
        .position(|argument| std::ptr::eq(argument.as_os_str(), script.as_os_str()))?;
    Some(&args[script_index + 1..])
}

fn classify_native(
    basename: &str,
    executable: Option<&Path>,
    command: &[OsString],
) -> Option<Provider> {
    match basename {
        "codex"
        | "codex-aarch64-apple-darwin"
        | "codex-x86_64-apple-darwin"
        | "codex-aarch64-unknown-linux-gnu"
        | "codex-x86_64-unknown-linux-gnu"
        | "codex-aarch64-unknown-linux-musl"
        | "codex-x86_64-unknown-linux-musl" => Some(Provider::Codex),
        "claude" | "claude-code" => Some(Provider::Claude),
        "gemini" => Some(Provider::Gemini),
        "opencode" => Some(Provider::OpenCode),
        "amp" => Some(Provider::Amp),
        "cursor-agent" => Some(Provider::Cursor),
        "agent" if executable.is_some_and(is_cursor_agent_path) => Some(Provider::Cursor),
        "copilot" => Some(Provider::Copilot),
        "aider" => Some(Provider::Aider),
        "goose" if is_verified_goose(executable, command) => Some(Provider::Goose),
        "cline" => Some(Provider::Cline),
        "qwen" => Some(Provider::QwenCode),
        "kilo" | "kilocode" => Some(Provider::Kilo),
        "droid" => Some(Provider::Droid),
        "crush" => Some(Provider::Crush),
        "pi" if executable.is_some_and(is_pi_path) => Some(Provider::Pi),
        "vibe" => Some(Provider::MistralVibe),
        _ => None,
    }
}

fn is_cursor_agent_path(path: &Path) -> bool {
    normalized(path).contains("/.local/share/cursor-agent/versions/")
}

fn is_verified_goose(executable: Option<&Path>, command: &[OsString]) -> bool {
    let Some(path) = executable else {
        return false;
    };
    let path = normalized(path);
    let credible_install = path.ends_with("/.local/bin/goose")
        || path.contains("/goose.app/contents/macos/")
        || path.contains("/aaif-goose/");
    credible_install
        && command_after_executable(command)
            .and_then(first_positional)
            .is_some_and(|value| value == "session")
}

fn is_pi_path(path: &Path) -> bool {
    let path = normalized(path);
    path.contains("/pi-coding-agent/") || path.contains("/.pi/agent/")
}

#[derive(Clone, Copy)]
enum Interpreter {
    Node,
    Bun,
    Python,
}

fn interpreter(basename: &str) -> Option<Interpreter> {
    if matches!(basename, "node" | "nodejs") {
        Some(Interpreter::Node)
    } else if basename == "bun" {
        Some(Interpreter::Bun)
    } else if basename == "python" || basename == "python3" || basename.starts_with("python3.") {
        Some(Interpreter::Python)
    } else {
        None
    }
}

fn interpreter_script(kind: Interpreter, command: &[OsString]) -> Option<&Path> {
    let args = command_after_executable(command)?;
    match kind {
        Interpreter::Node => node_script(args),
        Interpreter::Bun => bun_script(args),
        Interpreter::Python => python_script(args),
    }
}

fn command_after_executable(command: &[OsString]) -> Option<&[OsString]> {
    command.get(1..)
}

fn node_script(args: &[OsString]) -> Option<&Path> {
    let mut index = 0;
    while let Some(argument) = args.get(index).and_then(|value| value.to_str()) {
        if argument == "--" {
            return args.get(index + 1).map(Path::new);
        }
        if matches!(argument, "-e" | "--eval" | "-p" | "--print") {
            return None;
        }
        if matches!(
            argument,
            "-r" | "--require" | "--loader" | "--import" | "--conditions" | "--env-file"
        ) {
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            if argument.contains('=')
                || matches!(
                    argument,
                    "--enable-source-maps"
                        | "--no-warnings"
                        | "--trace-warnings"
                        | "--use-bundled-ca"
                        | "--use-openssl-ca"
                )
            {
                index += 1;
                continue;
            }
            return None;
        }
        return Some(Path::new(&args[index]));
    }
    None
}

fn bun_script(args: &[OsString]) -> Option<&Path> {
    let mut index = 0;
    while let Some(argument) = args.get(index).and_then(|value| value.to_str()) {
        if argument == "--" {
            return args.get(index + 1).map(Path::new);
        }
        if matches!(argument, "run" | "x" | "-e" | "--eval" | "-p" | "--print") {
            return None;
        }
        if matches!(argument, "-r" | "--preload" | "--cwd" | "--config") {
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            if argument.contains('=') || matches!(argument, "--bun" | "--silent" | "--smol") {
                index += 1;
                continue;
            }
            return None;
        }
        return Some(Path::new(&args[index]));
    }
    None
}

fn python_script(args: &[OsString]) -> Option<&Path> {
    let mut index = 0;
    while let Some(argument) = args.get(index).and_then(|value| value.to_str()) {
        if argument == "--" {
            return args.get(index + 1).map(Path::new);
        }
        if matches!(argument, "-c" | "-m") {
            return None;
        }
        if matches!(argument, "-W" | "-X") {
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            if matches!(
                argument,
                "-b" | "-B"
                    | "-d"
                    | "-E"
                    | "-i"
                    | "-I"
                    | "-O"
                    | "-OO"
                    | "-P"
                    | "-q"
                    | "-R"
                    | "-s"
                    | "-S"
                    | "-u"
                    | "-v"
                    | "-V"
                    | "-x"
            ) || argument.starts_with("-W")
                || argument.starts_with("-X")
            {
                index += 1;
                continue;
            }
            return None;
        }
        return Some(Path::new(&args[index]));
    }
    None
}

fn classify_script(path: &Path) -> Option<Provider> {
    let path = normalized(path);
    let basename = path.rsplit('/').next()?;

    if ((path.contains("/@google/gemini-cli/") || path.contains("/gemini-cli/packages/cli/"))
        && matches!(basename, "index.js" | "gemini.js"))
        || (path.contains("/cellar/gemini-cli/") && path.ends_with("/bin/gemini"))
    {
        Some(Provider::Gemini)
    } else if (path.contains("/opencode-ai/") || path.contains("/opencode/packages/opencode/"))
        && (path.ends_with("/bin/opencode") || basename == "index.ts")
    {
        Some(Provider::OpenCode)
    } else if path.contains("/@ampcode/cli/") && matches!(basename, "amp" | "cli.js") {
        Some(Provider::Amp)
    } else if path.contains("/@github/copilot/")
        && (path.ends_with("/bin/copilot") || basename == "index.js")
    {
        Some(Provider::Copilot)
    } else if (basename == "aider" && path.contains("/bin/"))
        || (basename == "aider.py" && path.contains("/aider/"))
    {
        Some(Provider::Aider)
    } else if is_cline_script(&path) {
        Some(Provider::Cline)
    } else if (path.contains("/@qwen-code/qwen-code/")
        || path.contains("/qwen-code/packages/cli/")
        || path.contains("/qwen-code/lib/"))
        && matches!(basename, "index.js" | "cli.js")
    {
        Some(Provider::QwenCode)
    } else if is_kilo_script(&path) {
        Some(Provider::Kilo)
    } else if path.contains("/node_modules/droid/") && matches!(basename, "droid" | "index.js") {
        Some(Provider::Droid)
    } else if path.contains("/@charmland/crush/") && basename == "crush" {
        Some(Provider::Crush)
    } else if (path.contains("/@earendil-works/pi-coding-agent/")
        || path.contains("/@mariozechner/pi-coding-agent/"))
        && basename == "cli.js"
    {
        Some(Provider::Pi)
    } else if (basename == "vibe" && path.contains("/bin/"))
        || (path.contains("/mistral-vibe/") && basename == "entrypoint.py")
    {
        Some(Provider::MistralVibe)
    } else {
        None
    }
}

fn is_cline_script(path: &str) -> bool {
    if path.ends_with("/node_modules/cline/bin/cline")
        || path.ends_with("/cline/apps/cli/src/index.ts")
    {
        return true;
    }
    let mut components = path.rsplit('/');
    components.next() == Some("cline")
        && components.next() == Some("bin")
        && components.next().is_some_and(|package| {
            matches!(
                package,
                "cli-darwin-arm64"
                    | "cli-darwin-x64"
                    | "cli-linux-arm64"
                    | "cli-linux-x64"
                    | "cli-windows-arm64"
                    | "cli-windows-x64"
            )
        })
        && components.next() == Some("@cline")
}

fn is_kilo_script(path: &str) -> bool {
    let mut components = path.rsplit('/');
    let Some(binary) = components.next() else {
        return false;
    };
    if !matches!(binary, "kilo" | "kilocode") || components.next() != Some("bin") {
        return false;
    }
    let Some(package) = components.next() else {
        return false;
    };
    if package == "opencode" {
        return path.contains("/kilocode/packages/opencode/bin/");
    }
    matches!(
        package,
        "cli"
            | "cli-darwin-arm64"
            | "cli-darwin-x64"
            | "cli-linux-arm64"
            | "cli-linux-x64"
            | "cli-windows-arm64"
            | "cli-windows-x64"
    ) && components.next() == Some("@kilocode")
}

fn classify_script_path(path: &Path) -> Option<Provider> {
    classify_script(path).or_else(|| {
        // One local filesystem resolution supports package-manager shims such
        // as Homebrew's bin/gemini symlink without scanning other files.
        path.is_absolute()
            .then(|| path.canonicalize().ok())
            .flatten()
            .and_then(|canonical| classify_script(&canonical))
    })
}

fn normalized(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

fn first_positional(args: &[OsString]) -> Option<&str> {
    args.iter()
        .filter_map(|value| value.to_str())
        .find(|value| !value.starts_with('-'))
}

fn option_before_positional(args: &[OsString], options: &[&str]) -> bool {
    let mut index = 0;
    while let Some(argument) = args.get(index).and_then(|value| value.to_str()) {
        if argument == "--" || !argument.starts_with('-') {
            return false;
        }
        if options.contains(&argument) {
            return true;
        }
        if matches!(
            argument,
            "-m" | "--model"
                | "--output-format"
                | "--approval-mode"
                | "--sandbox"
                | "--include-directories"
                | "--extensions"
        ) {
            index += 2;
        } else {
            index += 1;
        }
    }
    false
}

fn codex_subcommand(args: &[OsString]) -> Option<&str> {
    let mut index = 0;
    while let Some(argument) = args.get(index).and_then(|value| value.to_str()) {
        if argument == "--" {
            return None;
        }
        if matches!(
            argument,
            "-m" | "--model"
                | "-c"
                | "--config"
                | "-s"
                | "--sandbox"
                | "-a"
                | "--ask-for-approval"
                | "-C"
                | "--cd"
                | "--add-dir"
                | "--profile"
                | "--oss-provider"
        ) {
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            if argument.contains('=')
                || matches!(argument, "--oss" | "--search" | "--no-alt-screen")
            {
                index += 1;
                continue;
            }
            return None;
        }
        return Some(argument);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn classifies_verified_native_executables() {
        let cases = [
            ("codex", Provider::Codex),
            ("claude", Provider::Claude),
            ("gemini", Provider::Gemini),
            ("opencode", Provider::OpenCode),
            ("amp", Provider::Amp),
            ("cursor-agent", Provider::Cursor),
            ("copilot", Provider::Copilot),
            ("aider", Provider::Aider),
            ("cline", Provider::Cline),
            ("qwen", Provider::QwenCode),
            ("kilo", Provider::Kilo),
            ("kilocode", Provider::Kilo),
            ("droid", Provider::Droid),
            ("crush", Provider::Crush),
            ("vibe", Provider::MistralVibe),
        ];
        for (name, expected) in cases {
            assert_eq!(classify(OsStr::new(name), None, &[]), Some(expected));
        }
    }

    #[test]
    fn classifies_verified_interpreter_package_scripts() {
        let cases = [
            (
                "node",
                "/opt/node_modules/@google/gemini-cli/dist/index.js",
                Provider::Gemini,
            ),
            (
                "node",
                "/opt/node_modules/@qwen-code/qwen-code/dist/index.js",
                Provider::QwenCode,
            ),
            (
                "node",
                "/opt/node_modules/@github/copilot/index.js",
                Provider::Copilot,
            ),
            ("bun", "/opt/cline/apps/cli/src/index.ts", Provider::Cline),
            (
                "node",
                "/opt/node_modules/@mariozechner/pi-coding-agent/dist/bundle/cli.js",
                Provider::Pi,
            ),
            ("python3", "/venv/bin/aider", Provider::Aider),
            ("python3", "/venv/bin/vibe", Provider::MistralVibe),
        ];

        for (runtime, script, expected) in cases {
            let command = command(&[runtime, script, "user prompt"]);
            assert_eq!(
                classify(OsStr::new(runtime), Some(Path::new(runtime)), &command),
                Some(expected)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolves_one_package_manager_script_symlink() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("Cellar/gemini-cli/0.46.0/bin/gemini");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, "#!/usr/bin/env node\n").unwrap();
        let shim = temp.path().join("bin/gemini");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        symlink(&installed, &shim).unwrap();
        let command = vec![OsString::from("node"), shim.into_os_string()];

        assert_eq!(
            classify(
                OsStr::new("node"),
                Some(Path::new("/usr/bin/node")),
                &command
            ),
            Some(Provider::Gemini)
        );
    }

    #[test]
    fn skips_interpreter_flags_before_the_actual_script() {
        let node_command = command(&[
            "node",
            "--require",
            "/tmp/instrumentation.js",
            "/opt/node_modules/@google/gemini-cli/dist/index.js",
        ]);
        assert_eq!(
            classify(OsStr::new("node"), Some(Path::new("node")), &node_command),
            Some(Provider::Gemini)
        );
        let python_command = command(&["python3", "-u", "/venv/bin/aider"]);
        assert_eq!(
            classify(
                OsStr::new("python3"),
                Some(Path::new("python3")),
                &python_command
            ),
            Some(Provider::Aider)
        );
    }

    #[test]
    fn rejects_generic_interpreters_prompts_and_shell_mentions() {
        // Empty command metadata occurs for protected/exiting processes.
        assert!(command_after_executable(&[]).is_none());
        for name in ["node", "python3", "bun"] {
            assert!(classify(OsStr::new(name), None, &[]).is_none());
        }

        let node_prompt = command(&["node", "/tmp/server.js", "use gemini and aider"]);
        assert_eq!(
            classify(
                OsStr::new("node"),
                Some(Path::new("/usr/bin/node")),
                &node_prompt
            ),
            None
        );
        let python_code = command(&["python3", "-c", "run aider"]);
        assert_eq!(
            classify(
                OsStr::new("python3"),
                Some(Path::new("/usr/bin/python3")),
                &python_code
            ),
            None
        );
        let shell = command(&["sh", "-c", "gemini && qwen && opencode"]);
        assert_eq!(
            classify(OsStr::new("sh"), Some(Path::new("/bin/sh")), &shell),
            None
        );
        assert_eq!(classify(OsStr::new("my-codex-helper"), None, &[]), None);
        for script in [
            "/tmp/node_modules/@cline/cli-evil/postinstall.js",
            "/tmp/node_modules/@kilocode/client/index.ts",
        ] {
            let command = command(&["node", script]);
            assert_eq!(
                classify(
                    OsStr::new("node"),
                    Some(Path::new("/usr/bin/node")),
                    &command
                ),
                None
            );
        }
    }

    #[test]
    fn generic_names_require_provider_specific_evidence() {
        assert_eq!(classify(OsStr::new("agent"), None, &[]), None);
        assert_eq!(classify(OsStr::new("goose"), None, &[]), None);
        assert_eq!(classify(OsStr::new("pi"), None, &[]), None);
        assert_eq!(
            classify(
                OsStr::new("pi"),
                Some(Path::new("/tmp/pi")),
                &command(&["/tmp/pi"])
            ),
            None
        );
        assert_eq!(
            classify(
                OsStr::new("goose"),
                Some(Path::new("/tmp/goose")),
                &command(&["/tmp/goose", "session"])
            ),
            None
        );

        let cursor = Path::new(
            "/home/me/.local/share/cursor-agent/versions/2026.09.10-fd3934a/cursor-agent",
        );
        assert_eq!(
            classify(OsStr::new("agent"), Some(cursor), &command(&["agent"])),
            Some(Provider::Cursor)
        );

        let goose = Path::new("/home/me/.local/bin/goose");
        assert_eq!(
            classify(
                OsStr::new("goose"),
                Some(goose),
                &command(&["goose", "session"])
            ),
            Some(Provider::Goose)
        );
        assert_eq!(
            classify(
                OsStr::new("goose"),
                Some(goose),
                &command(&["goose", "postgres", "up"])
            ),
            None
        );
    }

    #[test]
    fn headless_detection_uses_cli_positions_only() {
        assert!(is_headless(
            Provider::Codex,
            &command(&["codex", "--model", "gpt-test", "exec", "task"])
        ));
        assert!(!is_headless(
            Provider::Codex,
            &command(&["codex", "resume", "exec"])
        ));
        assert!(!is_headless(
            Provider::Gemini,
            &command(&["gemini", "tell me to use --print"])
        ));
        assert!(!is_headless(
            Provider::Cursor,
            &command(&["cursor-agent", "--", "--print"])
        ));
        assert!(is_headless(
            Provider::Cursor,
            &command(&["cursor-agent", "--print", "task"])
        ));
        assert!(is_headless(
            Provider::Gemini,
            &command(&[
                "node",
                "/opt/node_modules/@google/gemini-cli/dist/index.js",
                "--prompt",
                "task",
            ])
        ));
        assert!(is_headless(
            Provider::Gemini,
            &command(&[
                "node",
                "--require",
                "/tmp/instrumentation.js",
                "/opt/node_modules/@google/gemini-cli/dist/index.js",
                "--prompt",
                "task",
            ])
        ));
        assert!(!is_headless(
            Provider::Gemini,
            &command(&[
                "gemini",
                "/opt/node_modules/@google/gemini-cli/dist/index.js",
                "--prompt",
            ])
        ));
        assert!(is_headless(
            Provider::Gemini,
            &command(&["gemini", "--model", "gemini-test", "--prompt", "task",])
        ));
    }
}
