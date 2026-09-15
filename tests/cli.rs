use serde_json::Value;
use std::process::Command;

fn command(dir: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ttybird"));
    c.arg("--config-dir").arg(dir);
    c.env("CODEX_HOME", dir.join("empty-codex"));
    c.env("CLAUDE_CONFIG_DIR", dir.join("empty-claude"));
    c.env("CLAUDE_HOME", dir.join("empty-claude"));
    c
}

#[test]
fn host_registration_round_trip_and_no_shell_injection() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        command(dir.path())
            .args(["hosts", "add", "lab", "--binary", "/opt/bin/ttybird"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = command(dir.path())
        .args(["hosts", "list"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value[0]["name"], "lab");
    assert!(
        !command(dir.path())
            .args(["hosts", "add", "lab", "--binary", "/wrong/bin/ttybird"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let unchanged = command(dir.path())
        .args(["hosts", "list"])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&unchanged.stdout).unwrap()[0]["binary"],
        "/opt/bin/ttybird"
    );
    assert!(
        !command(dir.path())
            .args(["hosts", "add", "evil; touch /tmp/should-not-exist"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        command(dir.path())
            .args(["hosts", "remove", "lab"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = command(dir.path())
        .args(["hosts", "list"])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        serde_json::json!([])
    );
}

#[test]
fn hooks_and_hook_failure_are_read_only_and_nonblocking() {
    let dir = tempfile::tempdir().unwrap();
    let out = command(dir.path()).arg("hooks").output().unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["hooks"]["Notification"].is_array());
    let failure = command(dir.path()).arg("hook").output().unwrap();
    assert!(failure.status.success());
    assert!(failure.stdout.is_empty());
    assert!(!dir.path().join("config.toml").exists());
}

#[test]
fn collector_protocol_and_read_only_state() {
    let dir = tempfile::tempdir().unwrap();
    let out = command(dir.path())
        .args(["collect", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["protocol_version"], 1);
    assert!(v["sessions"].is_array());
    assert!(!dir.path().join("config.toml").exists());
    for s in v["sessions"].as_array().unwrap() {
        assert!(s.get("prompt").is_none());
        assert!(s.get("command_line").is_none());
    }
}

#[test]
fn explicit_dashboard_refuses_pipes_without_terminal_escape_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let out = command(dir.path()).arg("tui").output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("interactive terminal"));
}

#[test]
fn provider_catalog_distinguishes_process_support_from_log_support() {
    let dir = tempfile::tempdir().unwrap();
    let out = command(dir.path())
        .args(["providers", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let providers: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    for name in [
        "codex",
        "claude",
        "gemini",
        "open_code",
        "amp",
        "cursor",
        "copilot",
        "aider",
        "goose",
        "cline",
        "qwen_code",
        "kilo",
        "droid",
        "crush",
        "pi",
        "mistral_vibe",
    ] {
        let entry = providers.iter().find(|p| p["provider"] == name).unwrap();
        assert_eq!(entry["process_discovery"], true);
        assert_eq!(
            entry["session_log_metadata"],
            matches!(name, "codex" | "claude")
        );
        if !matches!(name, "codex" | "claude") {
            assert!(entry["activity"].as_str().unwrap().contains("unknown"));
        }
    }
    assert!(!dir.path().join("config.toml").exists());
}
