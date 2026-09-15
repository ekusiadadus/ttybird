//! Real tmux integration, using only a fresh private server and synthetic output.
//! Run explicitly: cargo test --test tmux_preview -- --ignored
use std::process::Command;
use std::time::{Duration, Instant};
use ttybird::{
    model::{Activity, Confidence, Provider, Session, Target},
    navigation, terminal_preview,
};

struct Server(String);
impl Server {
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .args(["-L", &self.0])
            .args(args)
            .output()
            .expect("tmux installed")
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

#[test]
#[ignore = "requires local tmux; creates and removes a dedicated synthetic server"]
fn live_capture_preserves_pane_and_buffers_and_rejects_wrong_identity() {
    check_capture(false);
    check_capture(true);
}

fn check_capture(alternate: bool) {
    let mode = if alternate { "\x1b[?1049h" } else { "" };
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fixture.sh");
    std::fs::write(&script, format!("#!/bin/sh\nprintf '{mode}\x1b[2J\x1b[H\x1b[31mRED\x1b[0m synthetic preview\\r\\n日本語 second row\\r\\nOLD_VALUE\\rCR_OK\x1b[K\\r\\nerase me\x1b[2K\\rERASE_OK\\r\\n'\nexec sleep 60\n")).unwrap();
    let server = Server(format!(
        "ttybird-preview-test-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    let start = server.run(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-x",
        "80",
        "-y",
        "24",
        "-s",
        "fixture",
        "-P",
        "-F",
        "#{pane_id}|#{pane_tty}|#{pane_pid}",
        "sh",
        script.to_str().unwrap(),
    ]);
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let out = String::from_utf8(start.stdout).unwrap();
    let fields: Vec<_> = out.trim().split('|').collect();
    let pane = fields[0];
    let tty = fields[1];
    let pid: u32 = fields[2].parse().unwrap();
    let target = Target::Tmux {
        socket: Some(server.0.clone()),
        pane: pane.into(),
    };
    assert_eq!(
        navigation::target_tty(&target).unwrap().as_deref(),
        Some(tty)
    );
    let before = server.run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{pane_id}|#{pane_active}|#{pane_width}|#{pane_height}|#{pane_in_mode}",
    ]);
    let buffers_before = server.run(&["list-buffers"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let text = loop {
        let text = terminal_preview::capture_tmux(&target, tty).unwrap();
        if text.to_string().contains("RED synthetic preview") {
            break text;
        }
        assert!(Instant::now() < deadline, "fixture did not render");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(text.to_string().contains("日本語 second row"));
    assert!(text.to_string().contains("CR_OK"));
    assert!(!text.to_string().contains("OLD_VALUE"));
    assert!(text.to_string().contains("ERASE_OK"));
    assert!(!text.to_string().contains("erase me"));
    assert!(
        text.lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content.contains("RED") && span.style.fg.is_some())
    );
    assert!(terminal_preview::capture_tmux(&target, "/dev/not-the-agent").is_err());
    let mut session = Session {
        id: "synthetic-only".into(),
        provider: Provider::Codex,
        parent_id: None,
        host: "fixture-local".into(),
        pid: Some(pid),
        process_started_at: ttybird::collect::process_identity(pid),
        tty: Some(tty.into()),
        cwd: None,
        model: None,
        insights: Default::default(),
        activity: Activity::Unknown,
        confidence: Confidence::Unknown,
        evidence: "Synthetic sleep process, not a live agent".into(),
        updated_at: None,
        target: Some(target),
    };
    assert!(
        terminal_preview::capture(&session, "fixture-local")
            .unwrap()
            .to_string()
            .contains("RED")
    );
    assert!(terminal_preview::capture(&session, "other-host").is_err());
    session.process_started_at = Some(1);
    assert!(terminal_preview::capture(&session, "fixture-local").is_err());
    let after = server.run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{pane_id}|#{pane_active}|#{pane_width}|#{pane_height}|#{pane_in_mode}",
    ]);
    let buffers_after = server.run(&["list-buffers"]);
    assert_eq!(before.stdout, after.stdout);
    assert_eq!(buffers_before.stdout, buffers_after.stdout);
}
