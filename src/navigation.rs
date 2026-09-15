use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::Target;
use crate::remote::run_bounded;

#[cfg(target_os = "macos")]
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GhosttyTerminal {
    pub id: String,
    pub cwd: String,
    pub title: String,
}

#[cfg(target_os = "macos")]
const GHOSTTY_LIST_JXA: &str = r#"
const app = Application("Ghostty");
JSON.stringify(app.terminals().map(term => ({
  id: String(term.id()),
  cwd: String(term.workingDirectory()),
  title: String(term.name())
})));
"#;

#[cfg(any(target_os = "macos", test))]
const GHOSTTY_FOCUS_APPLESCRIPT: &str = r#"
on run argv
    if (count of argv) is not 1 then error "expected one terminal id"
    set terminalID to item 1 of argv
    tell application "Ghostty"
        if not (exists terminal id terminalID) then error "terminal does not exist"
        focus (terminal id terminalID)
    end tell
end run
"#;

#[cfg(target_os = "macos")]
pub fn ghostty_terminals() -> Result<Vec<GhosttyTerminal>> {
    let args = vec![
        "-l".to_owned(),
        "JavaScript".to_owned(),
        "-e".to_owned(),
        GHOSTTY_LIST_JXA.to_owned(),
    ];
    let output = run_bounded("osascript", &args, COMMAND_TIMEOUT, OUTPUT_LIMIT)
        .context("failed to query Ghostty terminals")?;
    let terminals: Vec<GhosttyTerminal> =
        serde_json::from_slice(&output).context("Ghostty returned invalid terminal data")?;
    for terminal in &terminals {
        validate_ghostty_id(&terminal.id).context("Ghostty returned an invalid terminal id")?;
    }
    Ok(terminals)
}

#[cfg(not(target_os = "macos"))]
pub fn ghostty_terminals() -> Result<Vec<GhosttyTerminal>> {
    bail!("Ghostty AppleScript navigation is available only on macOS")
}

fn validate_ghostty_id(id: &str) -> Result<()> {
    if id.len() != 36
        || !id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
    {
        bail!("Ghostty terminal id must be a canonical UUID");
    }
    Ok(())
}

fn validate_pane(pane: &str) -> Result<()> {
    if pane.len() < 2
        || !pane.starts_with('%')
        || !pane[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("tmux pane must be a %-prefixed numeric id");
    }
    Ok(())
}

pub(crate) fn socket_args(socket: Option<&str>) -> Result<Vec<String>> {
    match socket {
        None => Ok(vec!["-L".to_owned(), "default".to_owned()]),
        Some(value) if value.starts_with('/') => {
            if value.len() > 4096
                || value
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte == b'\0')
            {
                bail!("tmux socket path is invalid");
            }
            Ok(vec!["-S".to_owned(), value.to_owned()])
        }
        Some(value) => {
            if value.is_empty()
                || value.starts_with('-')
                || value.len() > 255
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            {
                bail!("tmux socket name is invalid");
            }
            Ok(vec!["-L".to_owned(), value.to_owned()])
        }
    }
}

pub fn validate_target(target: &Target) -> Result<()> {
    match target {
        Target::Ghostty { terminal_id } => validate_ghostty_id(terminal_id),
        Target::Tmux { socket, pane } => {
            socket_args(socket.as_deref())?;
            validate_pane(pane)
        }
    }
}

fn tmux_socket_from_env(value: &str) -> Result<String> {
    let mut pieces = value.rsplitn(3, ',');
    let pane_index = pieces.next().unwrap_or_default();
    let server_pid = pieces.next().unwrap_or_default();
    let socket = pieces.next().unwrap_or_default();
    if socket.is_empty()
        || !socket.starts_with('/')
        || !server_pid.bytes().all(|byte| byte.is_ascii_digit())
        || !pane_index.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("TMUX has an invalid socket descriptor");
    }
    socket_args(Some(socket))?;
    Ok(socket.to_owned())
}

fn parse_tmux_rows(output: &[u8], socket: Option<String>) -> Result<Vec<(String, Target)>> {
    let text = std::str::from_utf8(output).context("tmux returned non-UTF-8 output")?;
    let mut targets = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (tty, pane) = line
            .split_once('|')
            .context("tmux returned an invalid pane row")?;
        if tty.is_empty() || tty.bytes().any(|byte| byte.is_ascii_control()) {
            bail!("tmux returned an invalid pane tty");
        }
        validate_pane(pane).context("tmux returned an invalid pane id")?;
        targets.push((
            tty.to_owned(),
            Target::Tmux {
                socket: socket.clone(),
                pane: pane.to_owned(),
            },
        ));
    }
    Ok(targets)
}

fn query_tmux(socket: Option<&str>) -> Result<Vec<u8>> {
    // tmux sanitizes literal tab separators to '_' in the C locale. These
    // kernel TTY paths and numeric tmux IDs use a printable separator instead.
    let mut args = socket_args(socket)?;
    args.extend([
        "list-panes".to_owned(),
        "-a".to_owned(),
        "-F".to_owned(),
        "#{pane_tty}|#{pane_id}".to_owned(),
    ]);
    run_bounded("tmux", &args, Duration::from_secs(2), OUTPUT_LIMIT)
}

pub fn tmux_targets() -> Result<Vec<(String, Target)>> {
    // Distinguish a missing tmux binary from an ordinary "no server running"
    // response to list-panes.
    let version_args = vec!["-V".to_owned()];
    run_bounded("tmux", &version_args, Duration::from_secs(2), 1024)
        .context("tmux is unavailable")?;

    let mut by_tty = BTreeMap::new();
    if let Ok(output) = query_tmux(None) {
        for (tty, target) in parse_tmux_rows(&output, None)? {
            by_tty.insert(tty, target);
        }
    }

    if let Some(value) = env::var_os("TMUX") {
        let value = value
            .to_str()
            .context("TMUX contains non-UTF-8 characters")?;
        let socket = tmux_socket_from_env(value)?;
        if let Ok(output) = query_tmux(Some(&socket)) {
            for (tty, target) in parse_tmux_rows(&output, Some(socket.clone()))? {
                // The explicit current socket is more precise if it is also the
                // default server and therefore produced a duplicate row.
                by_tty.insert(tty, target);
            }
        }
    }
    Ok(by_tty.into_iter().collect())
}

#[cfg(target_os = "macos")]
fn focus_ghostty(terminal_id: &str) -> Result<()> {
    let args = vec![
        "-l".to_owned(),
        "AppleScript".to_owned(),
        "-e".to_owned(),
        GHOSTTY_FOCUS_APPLESCRIPT.to_owned(),
        "--".to_owned(),
        terminal_id.to_owned(),
    ];
    run_bounded("osascript", &args, COMMAND_TIMEOUT, 4096)
        .context("failed to focus Ghostty terminal")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn focus_ghostty(_terminal_id: &str) -> Result<()> {
    bail!("Ghostty AppleScript navigation is available only on macOS")
}

fn same_tmux_server(target_socket: Option<&str>, current_socket: &str) -> bool {
    match target_socket {
        Some(socket) if socket.starts_with('/') => socket == current_socket,
        Some(name) => {
            Path::new(current_socket)
                .file_name()
                .and_then(|v| v.to_str())
                == Some(name)
        }
        None => {
            Path::new(current_socket)
                .file_name()
                .and_then(|value| value.to_str())
                == Some("default")
        }
    }
}

fn tmux_checked_status(socket: Option<&str>, command_args: &[&str]) -> Result<()> {
    let mut args = socket_args(socket)?;
    args.extend(command_args.iter().map(|value| (*value).to_owned()));
    let status = Command::new("tmux")
        .args(&args)
        .status()
        .context("failed to start tmux")?;
    if !status.success() {
        bail!("tmux navigation failed ({status})");
    }
    Ok(())
}

struct ResolvedTmuxTarget {
    session: String,
    tty: String,
}

fn resolve_tmux_target(socket: Option<&str>, pane: &str) -> Result<ResolvedTmuxTarget> {
    let mut args = socket_args(socket)?;
    args.extend([
        "display-message".to_owned(),
        "-p".to_owned(),
        "-t".to_owned(),
        pane.to_owned(),
        "#{pane_id}|#{session_id}|#{pane_tty}".to_owned(),
    ]);
    let output = run_bounded("tmux", &args, Duration::from_secs(2), 1024)
        .context("tmux target does not exist")?;
    let row = std::str::from_utf8(&output)
        .context("tmux returned non-UTF-8 target data")?
        .trim_end();
    let mut fields = row.split('|');
    let resolved_pane = fields.next().context("tmux returned invalid target data")?;
    let session = fields.next().context("tmux returned invalid target data")?;
    let tty = fields.next().context("tmux returned invalid target data")?;
    if fields.next().is_some() {
        bail!("tmux returned invalid target data");
    }
    if resolved_pane != pane {
        bail!("tmux resolved a different pane");
    }
    if session.len() < 2
        || !session.starts_with('$')
        || !session[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("tmux returned an invalid session id");
    }
    if tty.is_empty() || tty.bytes().any(|byte| byte.is_ascii_control()) {
        bail!("tmux returned an invalid pane tty");
    }
    Ok(ResolvedTmuxTarget {
        session: session.to_owned(),
        tty: tty.to_owned(),
    })
}

/// Return the terminal device currently owned by an exact navigation target.
///
/// Ghostty's AppleScript API does not expose a terminal device, so it returns
/// `None`. A tmux target is resolved again by its opaque pane id on the selected
/// server; stale or malformed panes fail instead of returning inferred data.
pub fn target_tty(target: &Target) -> Result<Option<String>> {
    validate_target(target)?;
    match target {
        Target::Ghostty { .. } => Ok(None),
        Target::Tmux { socket, pane } => {
            Ok(Some(resolve_tmux_target(socket.as_deref(), pane)?.tty))
        }
    }
}

fn focus_tmux(socket: Option<&str>, pane: &str) -> Result<()> {
    let resolved = resolve_tmux_target(socket, pane)?;
    if let Some(value) = env::var_os("TMUX") {
        let current_socket = tmux_socket_from_env(
            value
                .to_str()
                .context("TMUX contains non-UTF-8 characters")?,
        )?;
        if !same_tmux_server(socket, &current_socket) {
            bail!("cannot switch a tmux client to a pane on another tmux server; detach first");
        }
        tmux_checked_status(socket, &["select-window", "-t", pane])?;
        tmux_checked_status(socket, &["select-pane", "-t", pane])?;
        tmux_checked_status(socket, &["switch-client", "-t", pane])
    } else {
        tmux_checked_status(socket, &["select-window", "-t", pane])?;
        tmux_checked_status(socket, &["select-pane", "-t", pane])?;
        tmux_checked_status(socket, &["attach-session", "-t", &resolved.session])
    }
}

pub fn focus(target: &Target) -> Result<()> {
    validate_target(target)?;
    match target {
        Target::Ghostty { terminal_id } => focus_ghostty(terminal_id),
        Target::Tmux { socket, pane } => focus_tmux(socket.as_deref(), pane),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_exact_ghostty_uuid() {
        let valid = Target::Ghostty {
            terminal_id: "D55DE9BA-D3E6-410A-8DE0-D7ACBF8253B6".to_owned(),
        };
        assert!(validate_target(&valid).is_ok());
        assert_eq!(target_tty(&valid).unwrap(), None);
        assert!(validate_ghostty_id("not-a-uuid").is_err());
        assert!(validate_ghostty_id("D55DE9BA-D3E6-410A-8DE0-D7ACBF8253BX").is_err());
    }

    #[test]
    fn validates_tmux_pane_and_socket() {
        assert!(
            validate_target(&Target::Tmux {
                socket: Some("/tmp/tmux-501/aoe".to_owned()),
                pane: "%12".to_owned(),
            })
            .is_ok()
        );
        assert!(
            validate_target(&Target::Tmux {
                socket: Some("-bad".to_owned()),
                pane: "%12".to_owned(),
            })
            .is_err()
        );
        assert!(
            validate_target(&Target::Tmux {
                socket: None,
                pane: "session:0.0".to_owned(),
            })
            .is_err()
        );
    }

    #[test]
    fn parses_tmux_environment_from_the_right() {
        let socket = tmux_socket_from_env("/tmp/with,comma/tmux-501/default,123,4").unwrap();
        assert_eq!(socket, "/tmp/with,comma/tmux-501/default");
        assert!(tmux_socket_from_env("malformed").is_err());
    }

    #[test]
    fn parses_only_exact_tmux_pane_rows() {
        let rows = parse_tmux_rows(b"/dev/ttys001|%3\n", None).unwrap();
        assert_eq!(
            rows,
            vec![(
                "/dev/ttys001".to_owned(),
                Target::Tmux {
                    socket: None,
                    pane: "%3".to_owned(),
                }
            )]
        );
        assert!(parse_tmux_rows(b"/dev/ttys001|work:0\n", None).is_err());
        assert!(parse_tmux_rows(b"/dev/ttys001|%3|extra\n", None).is_err());
        assert!(parse_tmux_rows(b"/dev/ttys001_%3\n", None).is_err());
    }
}
