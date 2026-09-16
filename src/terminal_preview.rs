//! Explicit, ephemeral preview of one verified local terminal.
//! Collection and JSON output never call this module.
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::text::Text;

use crate::{
    collect,
    model::{Session, Target},
    navigation,
    remote::run_bounded,
};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BYTES: usize = 1024 * 1024;

pub fn selection_key(session: &Session) -> String {
    serde_json::to_string(&(
        &session.host,
        &session.provider,
        &session.id,
        session.pid,
        session.process_started_at,
        &session.tty,
        &session.target,
    ))
    .expect("serializable session identity")
}

/// UI preflight only; capture still revalidates the process and pane.
pub fn unavailable_reason(session: &Session, local_host: &str) -> Option<&'static str> {
    if session.host != local_host {
        return Some("Remote screen preview is unavailable. Enter opens its mapped terminal.");
    }
    if session.pid.is_none() || session.process_started_at.is_none() {
        return Some("This entry has no verified live terminal to preview.");
    }
    match session.target {
        Some(Target::Tmux { .. }) if session.tty.is_some() => None,
        Some(Target::Managed { .. }) => None,
        Some(Target::Ghostty { .. }) if cfg!(target_os = "macos") => None,
        _ => Some(
            "No terminal mapping. Press g to link the correct Ghostty pane, then p to preview.",
        ),
    }
}

pub fn manual_snapshot(session: &Session) -> bool {
    matches!(session.target, Some(Target::Ghostty { .. }))
}

fn live_identity(session: &Session, require_tty: bool) -> Result<()> {
    let pid = session
        .pid
        .context("Preview needs a verified live process; this entry is history")?;
    let started = session
        .process_started_at
        .context("Process start time is unavailable")?;
    if collect::process_identity(pid) != Some(started) {
        bail!("Agent exited or PID was reused; refresh the session list");
    }
    if !require_tty {
        return Ok(());
    }
    // Check the current process device, not just the cached observation.
    let out = run_bounded(
        "ps",
        &["-p".into(), pid.to_string(), "-o".into(), "tty=".into()],
        TIMEOUT,
        1024,
    )?;
    let tty = std::str::from_utf8(&out)?.trim();
    let expected = session
        .tty
        .as_deref()
        .context("Agent terminal device is unavailable")?;
    if tty.is_empty()
        || tty == "?"
        || tty == "??"
        || normalized_tty(tty) != normalized_tty(expected)
    {
        bail!("Agent terminal changed or is unavailable; refresh the session list");
    }
    Ok(())
}

fn normalized_tty(tty: &str) -> &str {
    tty.strip_prefix("/dev/").unwrap_or(tty)
}

#[derive(Debug, PartialEq, Eq)]
struct Pane {
    cols: u16,
    rows: u16,
}

fn parse_pane(bytes: &[u8], pane: &str, tty: &str) -> Result<Pane> {
    let output = std::str::from_utf8(bytes)?;
    let line = output
        .strip_suffix('\n')
        .context("tmux pane response is incomplete")?;
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        bail!("tmux returned more than one pane record");
    }
    let fields: Vec<_> = line.split('|').collect();
    if fields.len() != 5 || fields[0] != pane || normalized_tty(fields[1]) != normalized_tty(tty) {
        bail!("tmux pane no longer matches the selected agent terminal");
    }
    if fields[4] != "0" {
        bail!("tmux pane is no longer running");
    }
    let cols: u16 = fields[2].parse().context("Invalid tmux width")?;
    let rows: u16 = fields[3].parse().context("Invalid tmux height")?;
    if !(1..=500).contains(&cols) || !(1..=200).contains(&rows) {
        bail!("Preview supports panes up to 500 columns by 200 rows");
    }
    Ok(Pane { cols, rows })
}

fn validate_capture_framing(bytes: &[u8], rows: u16) -> Result<()> {
    // capture-pane writes one LF-terminated record per requested screen row.
    // Missing or extra records are an incomplete/ambiguous snapshot, not
    // blank rows that the renderer should silently synthesize.
    if !bytes.ends_with(b"\n") {
        bail!("tmux screen capture is incomplete");
    }
    let captured_rows = bytes.iter().filter(|byte| **byte == b'\n').count();
    if captured_rows != usize::from(rows) {
        bail!("tmux screen capture returned {captured_rows} rows; expected {rows}");
    }
    Ok(())
}

fn query_pane(socket: Option<&str>, pane: &str, tty: &str) -> Result<Pane> {
    let mut args = navigation::socket_args(socket)?;
    args.extend([
        "display-message".into(),
        "-p".into(),
        "-t".into(),
        pane.into(),
        "#{pane_id}|#{pane_tty}|#{pane_width}|#{pane_height}|#{pane_dead}".into(),
    ]);
    let bytes = run_bounded("tmux", &args, TIMEOUT, 1024).context("tmux pane is unavailable")?;
    parse_pane(&bytes, pane, tty)
}

/// Captures only the visible screen, without changing tmux selection, buffers,
/// copy mode, size, history, or input. Caller must establish host and PID identity.
pub fn capture_tmux(target: &Target, tty: &str) -> Result<Text<'static>> {
    navigation::validate_target(target)?;
    let Target::Tmux { socket, pane } = target else {
        bail!(
            "Preview currently supports local tmux panes. Enter opens the mapped Ghostty terminal."
        );
    };
    let before = query_pane(socket.as_deref(), pane, tty)?;
    let mut args = navigation::socket_args(socket.as_deref())?;
    args.extend([
        "capture-pane".into(),
        "-p".into(),
        "-e".into(),
        "-N".into(),
        "-t".into(),
        pane.clone(),
        "-S".into(),
        "0".into(),
        "-E".into(),
        (before.rows - 1).to_string(),
    ]);
    let bytes =
        run_bounded("tmux", &args, TIMEOUT, MAX_BYTES).context("Unable to read tmux screen")?;
    let after = query_pane(socket.as_deref(), pane, tty)?;
    if before != after {
        bail!("Pane resized during capture; retrying on next refresh");
    }
    validate_capture_framing(&bytes, before.rows)?;
    crate::preview::parse(&bytes, before.cols, before.rows)
}

pub fn capture(session: &Session, local_host: &str) -> Result<Text<'static>> {
    if session.host != local_host {
        bail!("Remote preview is not available yet. Enter opens the remote terminal.");
    }
    let target = session
        .target
        .as_ref()
        .context("No tmux mapping. Run the agent in tmux or bind its exact pane.")?;
    navigation::validate_target(target)?;
    let require_tty = matches!(target, Target::Tmux { .. }) || session.tty.is_some();
    live_identity(session, require_tty)?;
    let screen = match target {
        Target::Tmux { .. } => {
            capture_tmux(target, session.tty.as_deref().context("Missing agent TTY")?)?
        }
        Target::Ghostty { terminal_id } => {
            let bytes = crate::ghostty_export::capture(terminal_id)?;
            crate::preview::parse_vt(&bytes, 120, 200)?
        }
        _ => bail!("This terminal does not support snapshot capture"),
    };
    live_identity(session, require_tty)?;
    Ok(screen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_exact_live_pane_record_and_rejects_invalid_state() {
        assert_eq!(
            parse_pane(b"%1|/dev/ttys004|80|24|0\n", "%1", "ttys004").unwrap(),
            Pane { cols: 80, rows: 24 }
        );
        assert_eq!(
            parse_pane(b"%1|ttys004|500|200|0\r\n", "%1", "/dev/ttys004").unwrap(),
            Pane {
                cols: 500,
                rows: 200
            }
        );
        for row in [
            "%2|/dev/ttys004|80|24|0\n",
            "%1|/dev/ttys005|80|24|0\n",
            "%1|/dev/ttys004|80|24|1\n",
            "%1|/dev/ttys004|0|24|0\n",
            "%1|/dev/ttys004|501|24|0\n",
            "%1|/dev/ttys004|80|201|0\n",
            "%1|/dev/ttys004|80|24|0",
            "%1|/dev/ttys004|80|24|0\n\n",
            "%1|/dev/ttys004|80|24|0 \n",
        ] {
            assert!(parse_pane(row.as_bytes(), "%1", "ttys004").is_err());
        }
    }

    #[test]
    fn requires_a_complete_visible_screen_row_set() {
        assert!(validate_capture_framing(b"first\nsecond\n", 2).is_ok());
        for (capture, rows) in [
            (b"".as_slice(), 2),
            (b"first\n".as_slice(), 2),
            (b"first\nsecond".as_slice(), 2),
            (b"first\nsecond\nthird\n".as_slice(), 2),
        ] {
            assert!(validate_capture_framing(capture, rows).is_err());
        }
    }

    #[test]
    fn ghostty_is_not_silently_treated_as_tmux() {
        let target = Target::Ghostty {
            terminal_id: "12345678-1234-1234-1234-123456789abc".into(),
        };
        assert!(capture_tmux(&target, "ttys004").is_err());
    }
}
