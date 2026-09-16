use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use ttybird::{
    collect::{self, CollectOptions},
    config, managed,
    model::{Activity, Binding, Confidence, Session, Snapshot, Target},
    navigation, remote, telemetry,
};

#[derive(Parser, Debug, Clone)]
#[command(
    version,
    about = "Find your coding agents. Return to their terminals.",
    long_about = "Find coding CLI processes across terminals and registered SSH hosts. Codex and Claude also provide session-log evidence. Collection never sends agent input. Explicit bindings enable terminal navigation."
)]
struct Cli {
    /// Print a static table instead of opening the interactive dashboard.
    #[arg(long, global = true, conflicts_with = "json")]
    plain: bool,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// Refresh snapshots; writes JSONL with --json. Ctrl-C exits cleanly.
    #[arg(long, global = true)]
    watch: bool,
    #[arg(long, default_value_t=3, value_parser=clap::value_parser!(u64).range(1..=3600), global=true)]
    interval: u64,
    /// Query only this computer; never connects over SSH.
    #[arg(long, global = true)]
    local: bool,
    /// Enable desktop notifications from observed attention events while running.
    #[arg(long, global = true)]
    notify: bool,
    /// Hide recent logs that could not be associated with a live process.
    #[arg(long, global = true)]
    live_only: bool,
    /// Include log-only history in the dashboard (toggle with h).
    #[arg(long, global = true, conflicts_with = "live_only")]
    history: bool,
    #[arg(long, default_value_t=15, value_parser=clap::value_parser!(u64).range(1..=1440), global=true)]
    recent_minutes: u64,
    #[arg(long, default_value_t=64, value_parser=clap::value_parser!(u64).range(1..=1024), global=true)]
    max_logs: u64,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Prepare, review and explicitly share a task handoff with a new Codex session.
    Handoff {
        #[command(subcommand)]
        command: HandoffCommand,
    },
    /// Read, acknowledge or snooze the observed attention inbox.
    Inbox {
        #[command(subcommand)]
        command: Option<InboxCommand>,
    },
    /// Start an owned terminal. Its program survives closing the dashboard.
    Run {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Reopen an owned terminal in the right pane.
    Attach {
        session: String,
    },
    /// List owned terminals (metadata only).
    Sessions,
    /// Stop only the selected TTYbird-owned terminal and its process group.
    Stop {
        session: String,
    },
    #[command(name = "__session-server", hide = true)]
    SessionServer {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Open the interactive dashboard (the default in a terminal).
    Tui,
    /// List live processes and recent session evidence (default).
    List,
    /// Only explicit, fresh requests for human input.
    NeedsMe,
    /// Local collector endpoint for registered SSH hosts; versioned JSON.
    Collect,
    /// Register SSH aliases explicitly. Uses your existing SSH configuration.
    Hosts {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Query Ghostty terminal IDs (macOS 1.3+; may require Automation access).
    Terminals,
    /// Bind an exact session to a terminal. No automatic cwd matching.
    Bind {
        session: String,
        /// Explicit live agent PID, required for unassociated recent logs.
        #[arg(long)]
        pid: Option<u32>,
        #[arg(long, conflicts_with = "tmux", required_unless_present = "tmux")]
        ghostty: Option<String>,
        #[arg(long, conflicts_with = "ghostty", required_unless_present = "ghostty")]
        tmux: Option<String>,
        /// Absolute tmux socket path (-S) or named socket (-L).
        #[arg(long, requires = "tmux")]
        socket: Option<String>,
    },
    Unbind {
        session: String,
    },
    /// Focus the bound Ghostty surface or attach to an existing tmux pane.
    Focus {
        session: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// Print an optional Claude settings fragment; never edits global settings.
    Hooks,
    /// Receive a Claude lifecycle hook. Saves only allowlisted metadata.
    #[command(hide = true)]
    Hook,
    /// Show adapter capabilities and configuration issues without launching agents.
    Doctor,
    /// List recognized coding CLI tools and their observation capabilities.
    Providers,
}

#[derive(Subcommand, Debug, Clone)]
enum HostCommand {
    Add {
        name: String,
        #[arg(long, default_value = "ttybird")]
        binary: String,
    },
    Remove {
        name: String,
    },
    List,
}

#[derive(Subcommand, Debug, Clone)]
enum HandoffCommand {
    Prepare {
        session: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        note: Vec<PathBuf>,
        #[arg(long)]
        include_conversation: bool,
    },
    Start {
        bundle: PathBuf,
        /// Confirm that you reviewed this exact draft and want to share it with Codex.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        detach: bool,
    },
}
#[derive(Subcommand, Debug, Clone)]
enum InboxCommand {
    Ack {
        id: String,
    },
    Snooze {
        id: String,
        #[arg(long, default_value_t=10, value_parser=clap::value_parser!(u64).range(1..=1440))]
        minutes: u64,
    },
}

fn options(cli: &Cli) -> CollectOptions {
    CollectOptions {
        recent_minutes: cli.recent_minutes,
        max_logs: cli.max_logs as usize,
        ..Default::default()
    }
}

fn local_snapshot(cli: &Cli, dir: &std::path::Path) -> Result<Snapshot> {
    let mut snapshot = collect::collect(&options(cli))?;
    if let Err(e) = telemetry::enrich(dir, &mut snapshot) {
        snapshot
            .warnings
            .push(format!("hook metadata unavailable: {e}"));
    }
    for binding in config::bindings(dir)? {
        let live_start = identity(binding.pid);
        for session in &mut snapshot.sessions {
            apply_binding(session, &binding, live_start);
        }
    }
    // tmux associates terminal devices, never working directories.
    if let Ok(panes) = navigation::tmux_targets() {
        for session in &mut snapshot.sessions {
            if session.target.is_some() {
                continue;
            }
            if let Some(tty) = &session.tty {
                let tty = tty.strip_prefix("/dev/").unwrap_or(tty);
                let matches: Vec<_> = panes
                    .iter()
                    .filter(|(p, _)| p.strip_prefix("/dev/").unwrap_or(p) == tty)
                    .collect();
                if matches.len() == 1 {
                    session.target = Some(matches[0].1.clone());
                }
            }
        }
    }
    if let Err(error) = enrich_managed(dir, &mut snapshot) {
        snapshot
            .warnings
            .push(format!("Owned terminals unavailable: {error}"));
    }
    ttybird::workflow::enrich_workspaces(&mut snapshot);
    Ok(snapshot)
}

fn enrich_managed(dir: &std::path::Path, snapshot: &mut Snapshot) -> Result<()> {
    for owned in managed::list(dir)? {
        if owned.ended {
            continue;
        }
        let target = Target::Managed {
            session_id: owned.id.clone(),
        };
        // CLI launchers can spawn a native executable. Match the exact child
        // or a process in its kernel PTY session, never directory proximity.
        // Preserve provider titles, usage and subagent descendants.
        if let Some(session) = snapshot.sessions.iter_mut().find(|s| {
            let owned_process =
                s.pid == Some(owned.pid) && s.process_started_at == Some(owned.process_started_at);
            let owned_pty = s.tty.as_deref().is_some_and(|tty| {
                tty.trim_start_matches("/dev/") == owned.tty.trim_start_matches("/dev/")
            }) && s.pid.is_some_and(|pid| {
                // SAFETY: getsid only queries an OS process session ID.
                unsafe { libc::getsid(pid as i32) == owned.pid as i32 }
            });
            (owned_process || owned_pty) && s.parent_id.is_none() && !s.id.contains("-pid-")
        }) {
            session.target = Some(target);
            continue;
        }
        snapshot
            .sessions
            .retain(|s| !(s.pid == Some(owned.pid) && s.id.contains("-pid-")));
        let provider = ttybird::model::Provider::ALL
            .iter()
            .copied()
            .find(|p| p.as_str().replace('_', "") == owned.program.replace('-', ""))
            .unwrap_or(ttybird::model::Provider::Unknown);
        let mut insights = ttybird::model::SessionInsights::default();
        insights.title = Some(owned.name.unwrap_or_else(|| owned.program.clone()));
        snapshot.sessions.push(Session {
            id: format!("managed-{}", owned.id),
            provider,
            parent_id: None,
            host: snapshot.host.clone(),
            pid: Some(owned.pid),
            process_started_at: Some(owned.process_started_at),
            tty: Some(owned.tty),
            cwd: Some(owned.cwd.to_string_lossy().into_owned()),
            model: None,
            activity: Activity::Unknown,
            confidence: Confidence::Observed,
            evidence: "TTYbird-owned PTY; process observed, model activity unknown".into(),
            updated_at: Some(snapshot.collected_at),
            target: Some(target),
            insights,
        });
    }
    Ok(())
}

fn identity(pid: u32) -> Option<u64> {
    collect::process_identity(pid)
}

fn apply_binding(session: &mut Session, binding: &Binding, live_start: Option<u64>) -> bool {
    // A persisted association is a navigation preference, never evidence that a
    // historical session is still attached to the backend process.
    if binding.host != session.host
        || binding.session_id != session.id
        || session.pid != Some(binding.pid)
        || session.process_started_at != Some(binding.process_started_at)
        || live_start != Some(binding.process_started_at)
    {
        return false;
    }
    session.target = Some(binding.target.clone());
    true
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn short(s: &str, n: usize) -> String {
    let s = clean(s);
    if s.chars().count() > n {
        format!(
            "{}…",
            s.chars().take(n.saturating_sub(1)).collect::<String>()
        )
    } else {
        s
    }
}

fn render(snapshots: &[Snapshot], needs_me: bool, live_only: bool, json: bool) -> Result<()> {
    let mut snapshots = snapshots.to_vec();
    for snapshot in &mut snapshots {
        snapshot.sessions.retain(|s| {
            (!live_only || s.pid.is_some())
                && (!needs_me
                    || (s.activity == Activity::WaitingInput
                        && s.confidence == Confidence::Observed))
        });
        snapshot.sessions.sort_by_key(|s| {
            (
                s.activity != Activity::WaitingInput,
                s.pid.is_none(),
                s.id.clone(),
            )
        });
    }
    if json {
        println!("{}", serde_json::to_string(&snapshots)?);
        return Ok(());
    }
    println!("TTYbird · coding agents across your terminals");
    println!(
        "{:<18} {:<13} {:<9} {:<16} {:<9} {:<15} {:<8} {:<18} WORKSPACE / SESSION",
        "HOST", "AGENT", "ROLE", "MODEL (LOG)", "PID", "STATE", "PROOF", "TERMINAL"
    );
    let mut count = 0;
    for snapshot in snapshots {
        for s in &snapshot.sessions {
            count += 1;
            let terminal = match &s.target {
                Some(Target::Ghostty { .. }) => "ghostty".to_string(),
                Some(Target::Managed { .. }) => "ttybird".to_string(),
                Some(Target::Tmux { pane, .. }) => format!("tmux {pane}"),
                None => s.tty.clone().unwrap_or_else(|| "—".into()),
            };
            println!(
                "{:<18} {:<13} {:<9} {:<16} {:<9} {:<15} {:<8} {:<18} {}",
                short(&s.host, 18),
                label(&s.provider),
                if s.parent_id.is_some() {
                    "subagent"
                } else if s.id.contains("-pid-") {
                    "process"
                } else {
                    "session"
                },
                short(s.model.as_deref().unwrap_or("—"), 16),
                s.pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "log only".into()),
                state(&s.activity),
                proof(&s.confidence),
                short(&terminal, 18),
                short(s.cwd.as_deref().unwrap_or("?"), 70)
            );
            println!("  {}  {}", clean(&s.id), short(&s.evidence, 110));
            if let Some(parent) = &s.parent_id {
                println!("  ↳ parent {}", clean(parent));
            }
        }
        for warning in &snapshot.warnings {
            eprintln!("{}: {}", clean(&snapshot.host), clean(warning));
        }
    }
    if count == 0 {
        println!("No matching observations. Unknown states are not counted as requests for input.");
    }
    println!(
        "PID = process observed; log only = recent history, liveness unverified. Proof: observed / inferred / unknown."
    );
    Ok(())
}

fn label(p: &ttybird::model::Provider) -> &str {
    p.as_str()
}

fn provider_capabilities() -> Vec<serde_json::Value> {
    ttybird::model::Provider::ALL.iter().map(|provider| {
        let logs = ttybird::providers::has_session_logs(*provider);
        serde_json::json!({
            "provider": provider, "name": provider.label(), "process_discovery": true,
            "session_log_metadata": logs,
            "activity": if logs { "provider evidence when available" } else { "unknown; process observation only" },
            "terminal_navigation": "requires verified mapping",
            "terminal_preview": "verified local tmux pane only"
        })
    }).collect()
}
fn state(s: &Activity) -> &str {
    match s {
        Activity::Working => "working",
        Activity::WaitingInput => "needs input",
        Activity::WaitingTool => "tool event",
        Activity::Idle => "idle",
        Activity::Ended => "ended",
        Activity::Unknown => "unknown",
    }
}
fn proof(s: &Confidence) -> &str {
    match s {
        Confidence::Observed => "observed",
        Confidence::Inferred => "inferred",
        Confidence::Unknown => "unknown",
    }
}

fn all_snapshots(cli: &Cli, dir: &std::path::Path) -> Result<Vec<Snapshot>> {
    let local = local_snapshot(cli, dir)?;
    let hosts = if cli.local {
        vec![]
    } else {
        config::read(dir)?.hosts
    };
    let mut result = vec![local];
    // Bounded fan-out: no more than four registered hosts are queried concurrently.
    for chunk in hosts.chunks(4) {
        std::thread::scope(|scope| {
            let jobs: Vec<_> = chunk
                .iter()
                .map(|h| {
                    scope.spawn(move || {
                        match remote::collect(&h.name, &h.binary, Duration::from_secs(8)) {
                            Ok(mut s) => {
                                s.host = h.name.clone();
                                for row in &mut s.sessions {
                                    row.host = h.name.clone();
                                }
                                s
                            }
                            Err(e) => {
                                let mut s = Snapshot::new(h.name.clone());
                                s.warnings.push(format!(
                                    "unreachable or invalid collector; agent state unknown: {e}"
                                ));
                                s
                            }
                        }
                    })
                })
                .collect();
            for job in jobs {
                if let Ok(s) = job.join() {
                    result.push(s);
                }
            }
        });
    }
    Ok(result)
}

fn find_session<'a>(snapshot: &'a Snapshot, id: &str) -> Result<&'a Session> {
    let matches: Vec<_> = snapshot.sessions.iter().filter(|s| s.id == id).collect();
    if matches.len() != 1 {
        bail!(
            "expected one exact session ID; found {} (use list --json)",
            matches.len()
        );
    }
    Ok(matches[0])
}

fn verify_picker_session(
    expected: &Session,
    current: &Session,
    live_start: Option<u64>,
) -> Result<()> {
    if expected.host != current.host
        || expected.provider != current.provider
        || expected.id != current.id
        || expected.pid.is_none()
        || expected.process_started_at.is_none()
        || expected.pid != current.pid
        || expected.process_started_at != current.process_started_at
        || expected.tty != current.tty
        || live_start != expected.process_started_at
    {
        bail!("session changed or process exited while choosing a pane; refresh and try again");
    }
    Ok(())
}

struct PendingBinding {
    saved: Binding,
    previous: Option<Binding>,
}

fn rollback_picker_binding(dir: &std::path::Path, pending: &PendingBinding) -> Result<()> {
    let _guard = config::mutation_guard(dir)?;
    let mut bindings = config::bindings(dir)?;
    let Some(index) = bindings.iter().position(|b| {
        b.host == pending.saved.host
            && b.session_id == pending.saved.session_id
            && b.pid == pending.saved.pid
            && b.process_started_at == pending.saved.process_started_at
            && b.target == pending.saved.target
    }) else {
        return Ok(());
    };
    bindings.remove(index);
    if let Some(previous) = &pending.previous {
        bindings.push(previous.clone());
    }
    config::save_bindings(dir, &bindings)
}

fn bind_picker_session(
    cli: &Cli,
    dir: &std::path::Path,
    expected: &Session,
    terminal_id: &str,
) -> Result<PendingBinding> {
    let target = Target::Ghostty {
        terminal_id: terminal_id.to_owned(),
    };
    navigation::validate_target(&target)?;
    if !navigation::ghostty_terminals()?
        .iter()
        .any(|term| term.id == terminal_id)
    {
        bail!("chosen Ghostty pane has closed; choose another pane");
    }
    let _guard = config::mutation_guard(dir)?;
    let snapshot = local_snapshot(cli, dir)?;
    if snapshot.host != expected.host {
        bail!("only local sessions can bind local Ghostty panes");
    }
    let current = find_session(&snapshot, &expected.id)?;
    verify_picker_session(expected, current, expected.pid.and_then(identity))?;
    let mut bindings = config::bindings(dir)?;
    let previous = bindings
        .iter()
        .find(|b| b.host == expected.host && b.session_id == expected.id)
        .cloned();
    bindings
        .retain(|binding| !(binding.host == expected.host && binding.session_id == expected.id));
    let saved = Binding {
        session_id: expected.id.clone(),
        host: expected.host.clone(),
        pid: expected.pid.context("missing live PID")?,
        process_started_at: expected
            .process_started_at
            .context("missing process start time")?,
        target,
    };
    bindings.push(saved.clone());
    config::save_bindings(dir, &bindings)?;
    Ok(PendingBinding { saved, previous })
}

fn focus(cli: &Cli, dir: &std::path::Path, id: &str, host: &Option<String>) -> Result<()> {
    focus_checked(cli, dir, id, host, false)
}

fn focus_checked(
    cli: &Cli,
    dir: &std::path::Path,
    id: &str,
    host: &Option<String>,
    ghostty_only: bool,
) -> Result<()> {
    if let Some(host) = host {
        let config = config::read(dir)?;
        let entry = config
            .hosts
            .iter()
            .find(|h| &h.name == host)
            .context("host is not registered")?;
        remote::validate_host(&entry.name)?;
        remote::validate_binary(&entry.binary)?;
        if !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:./".contains(&b))
            || id.starts_with('-')
            || id.len() > 256
        {
            bail!("unsafe remote session identifier");
        }
        let command = format!("{} focus '{}'", entry.binary, id);
        let status = std::process::Command::new("ssh")
            .args([
                "-t",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "--",
                &entry.name,
                &command,
            ])
            .status()?;
        if !status.success() {
            bail!("remote focus failed; session was not restarted");
        }
        return Ok(());
    }
    let snapshot = local_snapshot(cli, dir)?;
    let s = find_session(&snapshot, id)?;
    let pid = s
        .pid
        .context("session liveness is unverified; no current process/log association")?;
    if s.process_started_at.is_none() || identity(pid) != s.process_started_at {
        bail!("agent process exited or PID was reused; refusing stale navigation");
    }
    let target = s
        .target
        .as_ref()
        .context("no terminal mapping; use terminals then bind, or run inside tmux")?;
    if ghostty_only && !matches!(target, Target::Ghostty { .. }) {
        bail!("terminal binding changed; refusing to attach over the dashboard");
    }
    if let Some(target_tty) = navigation::target_tty(target)? {
        let process_tty = s
            .tty
            .as_deref()
            .context("process TTY unavailable; refusing unverified tmux focus")?;
        if process_tty.trim_start_matches("/dev/") != target_tty.trim_start_matches("/dev/") {
            bail!("tmux pane TTY differs from current agent TTY; rebind the session");
        }
    }
    navigation::focus(target)
}

type FocusResult = std::result::Result<Option<Binding>, String>;

// AppleScript switches a different surface; it does not need this terminal.
// Keep the dashboard and its event loop alive while revalidating and focusing.
fn focus_ghostty_from_dashboard(
    cli: &Cli,
    dir: &std::path::Path,
    id: String,
    pending: Option<PendingBinding>,
) -> std::sync::mpsc::Receiver<FocusResult> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let cli = cli.clone();
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        let result = match focus_checked(&cli, &dir, &id, &None, true) {
            Ok(()) => Ok(pending.map(|binding| binding.saved)),
            Err(error) => {
                let rollback = pending
                    .as_ref()
                    .map(|binding| rollback_picker_binding(&dir, binding))
                    .transpose();
                Err(match rollback {
                    Ok(_) => format!("{error:#}"),
                    Err(rollback) => format!("{error:#}; binding rollback failed: {rollback:#}"),
                })
            }
        };
        let _ = tx.send(result);
    });
    rx
}

/// Ratatui's `Terminal` destructor tries to show a cursor it previously hid.
/// On a revoked PTY that write fails, and Ratatui then reports the failure with
/// `eprintln!` to the same PTY, which can panic. Treat only confirmed terminal
/// disappearance as a successful no-op; every other output error still matters.
struct HangupSafeWriter<W> {
    inner: W,
}

impl<W> HangupSafeWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W: Write> Write for HangupSafeWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.inner.write(buffer) {
            Err(error) if terminal_gone_error(&error) => Ok(buffer.len()),
            result => result,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.inner.flush() {
            Err(error) if terminal_gone_error(&error) => Ok(()),
            result => result,
        }
    }
}

struct ScreenGuard {
    #[cfg(unix)]
    input_flags: libc::c_int,
}

impl ScreenGuard {
    fn enter() -> Result<Self> {
        #[cfg(unix)]
        // SAFETY: stdin is an open descriptor, verified before entering the TUI.
        let input_flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
        #[cfg(unix)]
        if input_flags == -1 {
            return Err(io::Error::last_os_error().into());
        }
        crossterm::terminal::enable_raw_mode()?;
        let guard = Self {
            #[cfg(unix)]
            input_flags,
        };
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableFocusChange,
            crossterm::cursor::Hide
        )?;
        Ok(guard)
    }

    fn poll_input(&self, timeout: Duration) -> io::Result<bool> {
        #[cfg(unix)]
        // The backend drains incomplete sequences until WouldBlock. Scope this
        // to input polling: stdin/stdout can share an open-file description,
        // and a nonblocking stdout would fail under terminal backpressure.
        // SAFETY: stdin belongs to this dashboard; the original flags are saved.
        if unsafe {
            libc::fcntl(
                libc::STDIN_FILENO,
                libc::F_SETFL,
                self.input_flags | libc::O_NONBLOCK,
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        let result = crossterm::event::poll(timeout);
        #[cfg(unix)]
        // SAFETY: restore flags before any drawing or returning to the shell.
        if unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, self.input_flags) } == -1 {
            return Err(io::Error::last_os_error());
        }
        result
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        #[cfg(unix)]
        // stdin can share its open-file description with the launching shell.
        // SAFETY: restore the exact status flags captured by this guard.
        unsafe {
            libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, self.input_flags);
        }
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::cursor::Show,
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableFocusChange,
            crossterm::terminal::LeaveAlternateScreen
        );
    }
}

/// Readiness only: never consume input here. Crossterm's bounded Unix backend
/// can spend the remainder of one poll on HUP, so check both before and after it.
/// Checking only before a poll would race a terminal closing during that call.
fn terminal_disconnected() -> io::Result<bool> {
    #[cfg(unix)]
    {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Ok(true);
        }
        let mut fds = [libc::STDIN_FILENO, libc::STDOUT_FILENO].map(|fd| libc::pollfd {
            fd,
            events: 0,
            revents: 0,
        });
        // SAFETY: fds is a valid array for the supplied length; timeout is zero.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if terminal_gone_error(&error) {
                return Ok(true);
            }
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(fds
            .iter()
            .any(|fd| fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0))
    }
    #[cfg(not(unix))]
    Ok(false)
}

fn terminal_gone_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
    ) {
        return true;
    }
    #[cfg(unix)]
    if matches!(
        error.raw_os_error(),
        Some(libc::EIO | libc::ENXIO | libc::ENOTTY)
    ) {
        return true;
    }
    false
}

type AttentionItems = Vec<ttybird::attention::AttentionItem>;
fn open_inbox(dir: &std::path::Path) -> Result<AttentionItems> {
    Ok(ttybird::attention::AttentionInbox::new(dir)
        .items(chrono::Utc::now().timestamp())?
        .into_iter()
        .filter(|item| item.is_open())
        .collect())
}
fn refresh_with_inbox(cli: &Cli, dir: &std::path::Path) -> Result<(Vec<Snapshot>, AttentionItems)> {
    let mut snapshots = all_snapshots(cli, dir)?;
    let inbox = ttybird::attention::AttentionInbox::new(dir);
    let now = chrono::Utc::now().timestamp();
    let result = inbox.update(&snapshots, now).and_then(|update| {
        if cli.notify {
            let report = inbox.notify_pending(now)?;
            if report.failed > 0 && let Some(local) = snapshots.first_mut() {
                local.warnings.push(format!("{} desktop notifications failed; retries use backoff. Check OS notification permissions and delivery tools.", report.failed));
            }
        }
        Ok(update
            .items
            .into_iter()
            .filter(|item| item.is_open())
            .collect())
    });
    let items = match result {
        Ok(items) => items,
        Err(error) => {
            if let Some(local) = snapshots.first_mut() {
                local
                    .warnings
                    .push(format!("Attention inbox unavailable: {error}"));
            }
            Vec::new()
        }
    };
    Ok((snapshots, items))
}

fn interactive(
    cli: &Cli,
    dir: &std::path::Path,
    needs_me: bool,
    initial_managed: Option<&str>,
) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::{sync::mpsc, time::Instant};
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("the dashboard needs an interactive terminal; use --plain or --json for pipes");
    }
    let running = Arc::new(AtomicBool::new(true));
    let signal = running.clone();
    ctrlc::set_handler(move || signal.store(false, Ordering::SeqCst))?;
    let guard = ScreenGuard::enter()?;
    let backend = ratatui::backend::CrosstermBackend::new(HangupSafeWriter::new(io::stdout()));
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut app = ttybird::ui::App {
        needs_only: needs_me,
        live_only: cli.live_only,
        show_history: cli.history,
        ..Default::default()
    };
    let mut initial_managed = initial_managed.map(str::to_owned);
    let owned_client = ttybird::managed_view::Client::new(dir);
    let mut owned_generation = 0u64;
    let mut owned_frame_busy = false;
    let mut owned_next_frame = Instant::now();
    let (requests, work) = mpsc::sync_channel::<()>(1);
    let (results, ready) =
        mpsc::sync_channel::<std::result::Result<(Vec<Snapshot>, AttentionItems), String>>(1);
    let worker_cli = cli.clone();
    let worker_dir = dir.to_path_buf();
    // One collector at a time. The UI never waits on SSH or process inspection.
    std::thread::spawn(move || {
        while work.recv().is_ok() {
            let result = refresh_with_inbox(&worker_cli, &worker_dir).map_err(|e| format!("{e:#}"));
            if results.send(result).is_err() {
                break;
            }
        }
    });
    requests.try_send(()).ok();
    app.refreshing = true;
    let mut last_request = Instant::now();
    let mut discard_refresh = false;
    // Preview is opt-in and independent of the metadata collector. Only owned
    // screen cells cross this channel; the !Send VT parser stays on its worker.
    let (preview_requests, preview_work) = mpsc::sync_channel::<(u64, String, Session, String)>(1);
    let (preview_results, preview_ready) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        while let Ok((generation, key, session, local_host)) = preview_work.recv() {
            let result = ttybird::terminal_preview::capture(&session, &local_host)
                .map_err(|e| format!("{e:#}"));
            if preview_results.send((generation, key, result)).is_err() {
                break;
            }
        }
    });
    let mut preview_key = None;
    let mut preview_generation = 0u64;
    let mut preview_local = false;
    let mut preview_busy = false;
    let mut next_preview = Instant::now();
    let mut destination = None;
    let mut focus_query: Option<mpsc::Receiver<FocusResult>> = None;
    type PaneQuery = (
        Session,
        std::result::Result<Vec<navigation::GhosttyTerminal>, String>,
    );
    let mut pane_query: Option<mpsc::Receiver<PaneQuery>> = None;
    let mut conversation_query: Option<(
        String,
        mpsc::Receiver<std::result::Result<String, String>>,
    )> = None;
    let mut conversation_key: Option<String> = None;
    enum HandoffResult {
        Draft(Box<ttybird::handoff::Draft>, Box<Session>),
        Conversation(String),
        Saved(PathBuf),
        Started(String),
    }
    let mut handoff_query: Option<mpsc::Receiver<std::result::Result<HandoffResult, String>>> =
        None;
    let mut redraw = true;
    while running.load(Ordering::SeqCst) {
        if terminal_disconnected()? {
            break;
        }
        if let Some(result) = handoff_query.as_ref().and_then(|rx| rx.try_recv().ok()) {
            handoff_query = None;
            redraw = true;
            match result {
                Ok(HandoffResult::Draft(draft, source)) => {
                    app.handoff = Some(ttybird::handoff_view::View::new(*draft, Some(*source)));
                    app.notice = None;
                }
                Ok(HandoffResult::Conversation(excerpt)) => {
                    if let Some(view) = &mut app.handoff {
                        match ttybird::handoff::add_conversation(&mut view.draft, &excerpt) {
                            Ok(()) => {
                                view.conversation_added = true;
                                app.notice =
                                    Some("Excerpt added. Review it before sharing.".into());
                            }
                            Err(error) => app.notice = Some(error.to_string()),
                        }
                    }
                }
                Ok(HandoffResult::Saved(path)) => {
                    app.notice = Some(format!(
                        "Private draft saved: {}",
                        clean(&path.to_string_lossy())
                    ))
                }
                Ok(HandoffResult::Started(id)) => {
                    app.handoff = None;
                    app.show_conversation = false;
                    app.show_preview = false;
                    initial_managed = Some(id);
                    app.notice = Some(
                        "Codex started with your reviewed handoff. Source session was not stopped."
                            .into(),
                    );
                    if !app.refreshing && requests.try_send(()).is_ok() {
                        app.refreshing = true;
                        last_request = Instant::now();
                    }
                }
                Err(error) => app.notice = Some(format!("Handoff: {}", clean(&error))),
            }
        }
        if let Some(result) = focus_query.as_ref().and_then(|rx| rx.try_recv().ok()) {
            focus_query = None;
            redraw = true;
            // A collector started before binding save/rollback must not put
            // the old navigation target back into the UI.
            discard_refresh = app.refreshing;
            if !app.refreshing && requests.try_send(()).is_ok() {
                app.refreshing = true;
                last_request = Instant::now();
            }
            app.notice = Some(match result {
                Ok(binding) => {
                    if let Some(binding) = binding {
                        for session in app.snapshots.iter_mut().flat_map(|s| &mut s.sessions) {
                            if session.host == binding.host
                                && session.id == binding.session_id
                                && session.pid == Some(binding.pid)
                                && session.process_started_at == Some(binding.process_started_at)
                            {
                                session.target = Some(binding.target.clone());
                            }
                        }
                    }
                    "Focused Ghostty. TTYbird stays open here; q quits.".into()
                }
                Err(error) => format!("Cannot focus terminal: {}", clean(&error)),
            });
        }
        if let Some((key, receiver)) = conversation_query.as_ref()
            && let Ok(result) = receiver.try_recv()
        {
            if app.show_conversation && Some(key) == conversation_key.as_ref() {
                app.conversation_text = Some(
                    result.unwrap_or_else(|error| format!("Conversation unavailable: {error}")),
                );
                redraw = true;
            }
            conversation_query = None;
        }
        if let Some((session, result)) = pane_query.as_ref().and_then(|rx| rx.try_recv().ok()) {
            redraw = true;
            pane_query = None;
            match result {
                Ok(mut terminals) if !terminals.is_empty() => {
                    // Rank same-directory choices for convenience, never bind by cwd alone.
                    terminals.sort_by_key(|term| {
                        (
                            session.cwd.as_deref() != Some(term.cwd.as_str()),
                            term.cwd.clone(),
                            term.id.clone(),
                        )
                    });
                    app.notice = None;
                    app.ghostty_picker = Some(ttybird::ui::GhosttyPicker {
                        session,
                        terminals,
                        selected: 0,
                    });
                }
                Ok(_) => {
                    app.notice = Some(
                        "No Ghostty panes found. Open Ghostty or use an explicit tmux binding."
                            .into(),
                    )
                }
                Err(error) => {
                    app.notice = Some(format!("Cannot list Ghostty panes: {}", clean(&error)))
                }
            }
        }
        while let Ok(result) = ready.try_recv() {
            redraw = true;
            app.refreshing = false;
            if discard_refresh {
                discard_refresh = false;
                if requests.try_send(()).is_ok() {
                    app.refreshing = true;
                    last_request = Instant::now();
                }
                continue;
            }
            match result {
                Ok((snapshots, items)) => {
                    app.set_snapshots(snapshots);
                    app.attention = items;
                }
                Err(error) => {
                    // Discard queries carrying a frozen pre-failure process
                    // identity and reject any in-flight preview response.
                    pane_query = None;
                    preview_generation = preview_generation.wrapping_add(1);
                    app.invalidate_liveness();
                    app.notice = Some(format!(
                        "Refresh failed; live status unavailable: {}",
                        clean(&error)
                    ))
                }
            }
        }
        if let Some(id) = initial_managed.as_deref() {
            let session = app.snapshots.iter().flat_map(|s| &s.sessions).find(|s| {
                matches!(&s.target, Some(Target::Managed { session_id }) if session_id == id)
            }).cloned();
            if let Some(session) = session {
                app.restore_selection(Some(&session));
                initial_managed = None;
            }
        }
        let owned_id = app
            .selected_session()
            .and_then(|s| ttybird::ui::navigation_session(s, &app.snapshots))
            .filter(|s| {
                app.snapshots
                    .first()
                    .is_some_and(|local| s.host == local.host)
            })
            .and_then(|s| match &s.target {
                Some(Target::Managed { session_id }) => Some(session_id.clone()),
                _ => None,
            });
        app.managed_parent = app.selected_session().is_some_and(|selected| {
            ttybird::ui::navigation_session(selected, &app.snapshots).is_some_and(|target| {
                target.id != selected.id && matches!(target.target, Some(Target::Managed { .. }))
            })
        });
        if owned_id != app.managed_id {
            app.managed_id = owned_id;
            app.managed_text = None;
            app.managed_cursor = None;
            app.managed_notice = None;
            app.terminal_input = false;
            owned_generation = owned_generation.wrapping_add(1);
            owned_next_frame = Instant::now();
            redraw = true;
        }
        while let Ok(reply) = owned_client.replies.try_recv() {
            if reply.frame {
                owned_frame_busy = false;
            }
            if reply.generation != owned_generation {
                continue;
            }
            match reply.result {
                Ok(ttybird::managed_view::Update::Frame {
                    text,
                    cursor,
                    ended,
                }) => {
                    if app.managed_text.as_ref() != Some(&text) || app.managed_cursor != cursor {
                        app.managed_text = Some(text);
                        app.managed_cursor = cursor;
                        redraw = true;
                    }
                    if ended {
                        app.terminal_input = false;
                        app.managed_cursor = None;
                        app.notice = Some("Program exited.".into());
                        redraw = true;
                    }
                }
                Ok(ttybird::managed_view::Update::Sent) => {}
                Err(error) => {
                    app.terminal_input = false;
                    app.managed_text = None;
                    app.managed_cursor = None;
                    app.managed_notice = Some(clean(&error));
                    owned_next_frame = Instant::now() + Duration::from_secs(2);
                    redraw = true;
                }
            }
        }
        if !owned_frame_busy
            && Instant::now() >= owned_next_frame
            && !app.show_conversation
            && !app.show_details
            && !app.show_help
            && app.handoff.is_none()
            && !app.show_inbox
            && let Some(id) = app.managed_id.as_deref()
        {
            let size = terminal.size()?;
            let (cols, rows) = ttybird::ui::managed_size(ratatui::layout::Rect::new(
                0,
                0,
                size.width,
                size.height,
            ));
            if owned_client
                .submit(owned_generation, id, managed::Request::Frame { cols, rows })
                .is_ok()
            {
                owned_frame_busy = true;
            }
            owned_next_frame =
                Instant::now() + Duration::from_millis(if app.terminal_input { 40 } else { 250 });
        }
        // Refresh may replace the selection: clear old text before this frame is drawn.
        let selected_key = app
            .selected_session()
            .map(ttybird::terminal_preview::selection_key);
        if app.show_conversation && selected_key != conversation_key {
            app.show_conversation = false;
            app.conversation_text = None;
            conversation_query = None;
            redraw = true;
        }
        if !app.refreshing && last_request.elapsed() >= Duration::from_secs(cli.interval) {
            if requests.try_send(()).is_ok() {
                app.refreshing = true;
                redraw = true;
            }
            last_request = Instant::now();
        }
        let selected = (app.show_preview
            && app.managed_id.is_none()
            && !app.liveness_unavailable
            && app.ghostty_picker.is_none()
            && pane_query.is_none()
            && app.handoff.is_none()
            && !app.show_inbox)
            .then(|| app.selected_session().cloned())
            .flatten();
        let selected_is_local = app.selected_session().is_some_and(|selected| {
            app.snapshots.first().is_some_and(|snapshot| {
                snapshot
                    .sessions
                    .iter()
                    .any(|session| std::ptr::eq(session, selected))
            })
        });
        let key = selected
            .as_ref()
            .map(ttybird::terminal_preview::selection_key);
        if key != preview_key || selected_is_local != preview_local {
            redraw = true;
            preview_generation = preview_generation.wrapping_add(1);
            preview_local = selected_is_local;
            preview_key = key;
            app.preview_text = None;
            app.preview_notice = Some(
                if selected.is_some() {
                    "Reading selected tmux screen…"
                } else {
                    "Select a local tmux agent to preview its terminal."
                }
                .into(),
            );
            app.preview_scroll = 0;
            next_preview = Instant::now();
        }
        while let Ok((generation, key, result)) = preview_ready.try_recv() {
            redraw = true;
            preview_busy = false;
            // A slow capture for a previous selection must never paint the new one.
            if app.show_preview
                && !app.liveness_unavailable
                && selected_is_local
                && preview_generation == generation
                && preview_key.as_ref() == Some(&key)
            {
                match result {
                    Ok(text) => {
                        app.preview_scroll = app
                            .preview_scroll
                            .min(text.lines.len().saturating_sub(1) as u16);
                        app.preview_text = Some(text);
                        app.preview_notice = Some(format!(
                            "Updated {} · visible screen only",
                            chrono::Local::now().format("%H:%M:%S")
                        ));
                    }
                    Err(error) => {
                        app.preview_text = None;
                        app.preview_notice = Some(clean(&error));
                    }
                }
            }
        }
        if let Some(session) = selected
            && !preview_busy
            && Instant::now() >= next_preview
        {
            let local_host = app
                .snapshots
                .first()
                .map(|s| s.host.clone())
                .unwrap_or_default();
            if !selected_is_local {
                app.preview_text = None;
                app.preview_notice = Some(
                    "Remote preview is not available yet. Enter opens the remote terminal.".into(),
                );
                next_preview = Instant::now() + Duration::from_secs(2);
            } else if preview_requests
                .try_send((
                    preview_generation,
                    preview_key.clone().unwrap(),
                    session,
                    local_host,
                ))
                .is_ok()
            {
                preview_busy = true;
                next_preview = Instant::now() + Duration::from_secs(2);
            }
        }
        if redraw {
            if let Err(error) = terminal.draw(|frame| ttybird::ui::draw(frame, &mut app)) {
                if terminal_gone_error(&error) || terminal_disconnected()? {
                    break;
                }
                return Err(error.into());
            }
            redraw = false;
        }
        let available = guard.poll_input(Duration::from_millis(if app.terminal_input {
            16
        } else {
            80
        }));
        if terminal_disconnected()? || !running.load(Ordering::SeqCst) {
            break;
        }
        if available.as_ref().is_err_and(terminal_gone_error) {
            break;
        }
        if !available? {
            continue;
        }
        redraw = true;
        let input_event = event::read()?;
        if input_event == Event::FocusLost {
            app.terminal_input = false;
            continue;
        }
        if app.terminal_input {
            // Legacy terminals send Ctrl+] as 0x1d, which crossterm 0.28
            // decodes as Ctrl+5. Both spellings represent the same escape key.
            let exit_input = matches!(&input_event, Event::Key(key) if matches!(key.code, KeyCode::Char(']' | '5')) && key.modifiers.contains(KeyModifiers::CONTROL));
            if exit_input {
                app.terminal_input = false;
                continue;
            }
            let request = match input_event {
                Event::Key(key) => {
                    ttybird::managed_view::key(key).map(|key| managed::Request::Key { key })
                }
                Event::Paste(text) if text.len() <= 8192 => Some(managed::Request::Paste { text }),
                Event::Paste(_) => {
                    app.notice = Some("Paste exceeds 8 KiB; not sent.".into());
                    None
                }
                _ => None,
            };
            if let (Some(id), Some(request)) = (app.managed_id.as_deref(), request)
                && let Err(error) = owned_client.submit(owned_generation, id, request)
            {
                app.notice = Some(error.to_string());
                app.terminal_input = false;
            }
            continue;
        }
        if app.handoff.is_some() {
            // Freeze the reviewed draft while saving/launching; a late excerpt
            // cannot be applied to a different handoff.
            if handoff_query.is_some() {
                continue;
            }
            use ttybird::handoff_view::Action;
            let action = app.handoff.as_mut().unwrap().event(input_event);
            match action {
                Action::Close => app.handoff = None,
                Action::None => {}
                action => {
                    let view = app.handoff.as_ref().unwrap();
                    let draft = view.draft.clone();
                    let source = view.source.clone();
                    let path = dir.to_path_buf();
                    let (tx, rx) = mpsc::sync_channel(1);
                    handoff_query = Some(rx);
                    app.notice = Some("Preparing handoff action…".into());
                    std::thread::spawn(move || {
                        let result = match action {
                            Action::Save => {
                                ttybird::handoff::save(&path, &draft).map(HandoffResult::Saved)
                            }
                            Action::Start => ttybird::handoff::start(&path, &draft)
                                .map(|s| HandoffResult::Started(s.id)),
                            Action::Conversation => source
                                .as_ref()
                                .context("No verified source session")
                                .and_then(collect::recent_conversation)
                                .map(HandoffResult::Conversation),
                            _ => unreachable!(),
                        }
                        .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(result);
                    });
                }
            }
            continue;
        }
        let Event::Key(key) = input_event else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            break;
        }
        if app.show_inbox {
            let now = chrono::Utc::now().timestamp();
            let inbox = ttybird::attention::AttentionInbox::new(dir);
            let item = app.attention.get(app.inbox_selected).cloned();
            let mut enter_session = None;
            let result = match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('N') => {
                    app.show_inbox = false;
                    Ok(())
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.inbox_selected =
                        (app.inbox_selected + 1).min(app.attention.len().saturating_sub(1));
                    Ok(())
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.inbox_selected = app.inbox_selected.saturating_sub(1);
                    Ok(())
                }
                KeyCode::Char('m') => item
                    .as_ref()
                    .map_or(Ok(()), |i| inbox.acknowledge(&i.id, now).map(|_| ())),
                KeyCode::Char('z') => item
                    .as_ref()
                    .map_or(Ok(()), |i| inbox.snooze(&i.id, now + 600).map(|_| ())),
                KeyCode::Enter => {
                    if let Some(item) = item {
                        enter_session = app
                            .snapshots
                            .iter()
                            .flat_map(|s| &s.sessions)
                            .find(|s| {
                                s.host == item.host
                                    && s.provider == item.provider
                                    && s.id == item.session_id
                                    && s.pid == Some(item.pid)
                                    && s.process_started_at == Some(item.process_started_at)
                            })
                            .cloned();
                        if enter_session.is_none() {
                            app.notice = Some(
                                "That event has no currently verified session to open.".into(),
                            );
                        }
                    }
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(error) = result {
                app.notice = Some(error.to_string());
            }
            if let Ok(items) = open_inbox(dir) {
                app.attention = items;
                app.inbox_selected = app
                    .inbox_selected
                    .min(app.attention.len().saturating_sub(1));
            }
            if let Some(session) = enter_session {
                app.show_inbox = false;
                app.needs_only = false;
                app.query.clear();
                app.show_background = true;
                app.collapsed.clear();
                app.restore_selection(Some(&session));
                if app
                    .selected_session()
                    .is_none_or(|s| s.id != session.id || s.host != session.host)
                {
                    app.notice = Some(
                        "Session is in a folded branch; expand it to open its terminal.".into(),
                    );
                    continue;
                }
                // Continue into the ordinary, revalidated Enter path.
            } else {
                continue;
            }
        }
        if app.ghostty_picker.is_some() {
            match key.code {
                KeyCode::Esc => {
                    app.ghostty_picker = None;
                    app.notice = Some("Pane selection cancelled; no binding changed.".into());
                }
                KeyCode::Char('q') => break,
                KeyCode::Down | KeyCode::Char('j') => {
                    let picker = app.ghostty_picker.as_mut().unwrap();
                    picker.selected = picker
                        .selected
                        .saturating_add(1)
                        .min(picker.terminals.len().saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let picker = app.ghostty_picker.as_mut().unwrap();
                    picker.selected = picker.selected.saturating_sub(1);
                }
                KeyCode::Enter => {
                    let choice = app.ghostty_picker.as_ref().and_then(|picker| {
                        picker
                            .terminals
                            .get(picker.selected)
                            .map(|term| (picker.session.clone(), term.id.clone()))
                    });
                    if let Some((session, terminal_id)) = choice {
                        match bind_picker_session(cli, dir, &session, &terminal_id) {
                            Ok(pending) => {
                                app.ghostty_picker = None;
                                app.notice = Some("Focusing Ghostty…".into());
                                focus_query = Some(focus_ghostty_from_dashboard(
                                    cli,
                                    dir,
                                    session.id,
                                    Some(pending),
                                ));
                            }
                            Err(error) => {
                                app.ghostty_picker = None;
                                app.notice = Some(format!(
                                    "Cannot bind pane: {}",
                                    clean(&format!("{error:#}"))
                                ));
                            }
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if app.searching {
            let previous_selection = app.selected_session().cloned();
            match key.code {
                KeyCode::Esc => {
                    app.searching = false;
                    app.query.clear();
                }
                KeyCode::Enter => app.searching = false,
                KeyCode::Backspace => {
                    app.query.pop();
                }
                KeyCode::Char(c)
                    if !c.is_control()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    app.query.push(c)
                }
                _ => {}
            }
            app.restore_selection(previous_selection.as_ref());
            continue;
        }
        if key.code == KeyCode::Char('q') {
            break;
        }
        if app.show_details {
            match key.code {
                KeyCode::Esc | KeyCode::Char('d') => app.show_details = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    app.detail_scroll = app.detail_scroll.saturating_add(1).min(200)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.detail_scroll = app.detail_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => {
                    app.detail_scroll = app.detail_scroll.saturating_add(8).min(200)
                }
                KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(8),
                KeyCode::Home => app.detail_scroll = 0,
                _ => {}
            }
            continue;
        }
        if app.show_help {
            app.show_help = false;
            continue;
        }
        let previous_selection = app.selected_session().cloned();
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Esc => {
                app.show_conversation = false;
                app.conversation_text = None;
                conversation_query = None;
                pane_query = None;
                preview_generation = preview_generation.wrapping_add(1);
                app.show_preview = false;
                app.preview_text = None;
                app.query.clear();
                app.notice = None;
                app.restore_selection(previous_selection.as_ref());
            }
            KeyCode::Char('N') => {
                let inbox = ttybird::attention::AttentionInbox::new(dir);
                match inbox
                    .mark_all_read(chrono::Utc::now().timestamp())
                    .and_then(|_| open_inbox(dir))
                {
                    Ok(items) => {
                        app.attention = items;
                        app.inbox_selected = 0;
                        app.show_inbox = true;
                    }
                    Err(error) => app.notice = Some(format!("Inbox: {error}")),
                }
            }
            KeyCode::Char('H') => {
                if handoff_query.is_some() {
                    app.notice = Some("A handoff action is still running.".into());
                    continue;
                }
                let source = app.selected_session().cloned();
                if let Some(source) = source {
                    if app.snapshots.first().is_none_or(|s| s.host != source.host) {
                        app.notice = Some("Prepare a handoff on the source host; remote file reads are not supported.".into());
                        continue;
                    }
                    let (tx, rx) = mpsc::sync_channel(1);
                    handoff_query = Some(rx);
                    app.notice =
                        Some("Preparing metadata-only draft; conversation is opt-in.".into());
                    std::thread::spawn(move || {
                        let result = source
                            .cwd
                            .as_deref()
                            .context("No workspace recorded")
                            .and_then(|cwd| {
                                ttybird::handoff::prepare(
                                    Some(&source),
                                    std::path::Path::new(cwd),
                                    &[],
                                    None,
                                )
                            })
                            .map(|draft| HandoffResult::Draft(Box::new(draft), Box::new(source)))
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(result);
                    });
                } else {
                    app.notice = Some("Select a local session to prepare a handoff, or use handoff prepare --cwd PATH.".into());
                }
            }
            KeyCode::Char('?') => app.show_help = true,
            KeyCode::Char('p') => {
                if app.managed_id.is_some() {
                    app.notice = Some("Owned terminal is already visible; Enter or i enables input, Ctrl+] returns here.".into());
                    continue;
                }
                app.show_conversation = false;
                app.conversation_text = None;
                conversation_query = None;
                preview_generation = preview_generation.wrapping_add(1);
                app.show_preview = !app.show_preview;
                app.preview_text = None;
                preview_key = None;
            }
            KeyCode::Char('c') => {
                app.show_conversation = !app.show_conversation;
                app.conversation_text = None;
                app.show_preview = false;
                app.preview_text = None;
                app.preview_scroll = 0;
                conversation_query = None;
                conversation_key = app
                    .selected_session()
                    .map(ttybird::terminal_preview::selection_key);
                if app.show_conversation
                    && let Some(session) = app.selected_session().cloned()
                {
                    let local = app
                        .snapshots
                        .first()
                        .is_some_and(|snapshot| snapshot.host == session.host);
                    if !local || app.liveness_unavailable {
                        app.conversation_text = Some(
                            "Only a currently verified local session can show recent messages."
                                .into(),
                        );
                    } else {
                        let (sender, receiver) = mpsc::sync_channel(1);
                        conversation_query = Some((conversation_key.clone().unwrap(), receiver));
                        std::thread::spawn(move || {
                            let result = collect::recent_conversation(&session)
                                .map_err(|error| format!("{error:#}"));
                            let _ = sender.send(result);
                        });
                    }
                }
            }
            KeyCode::Char('d') => {
                app.show_details = true;
                app.detail_scroll = 0;
            }
            KeyCode::Char('/') => app.searching = true,
            KeyCode::Char(' ') => app.toggle_branch(),
            KeyCode::Left => app.collapse_or_parent(),
            KeyCode::Right => app.expand_or_child(),
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            KeyCode::PageDown if app.show_conversation => {
                app.preview_scroll = app.preview_scroll.saturating_add(8).min(200);
            }
            KeyCode::PageUp if app.show_conversation => {
                app.preview_scroll = app.preview_scroll.saturating_sub(8);
            }
            KeyCode::PageDown if app.show_preview => {
                let limit = app
                    .preview_text
                    .as_ref()
                    .map(|t| t.lines.len().saturating_sub(1) as u16)
                    .unwrap_or(0);
                app.preview_scroll = app.preview_scroll.saturating_add(8).min(limit);
            }
            KeyCode::PageUp if app.show_preview => {
                app.preview_scroll = app.preview_scroll.saturating_sub(8)
            }
            KeyCode::PageDown => app.move_selection(10),
            KeyCode::PageUp => app.move_selection(-10),
            KeyCode::Home => {
                app.selected = 0;
                app.move_selection(0);
            }
            KeyCode::End => {
                app.selected = app.rows().len().saturating_sub(1);
            }
            KeyCode::Char('a') => {
                app.needs_only = !app.needs_only;
                app.restore_selection(previous_selection.as_ref());
            }
            KeyCode::Char('h') => {
                app.show_history = !app.show_history;
                app.restore_selection(previous_selection.as_ref());
            }
            KeyCode::Char('b') => {
                app.show_background = !app.show_background;
                app.restore_selection(previous_selection.as_ref());
            }
            KeyCode::Char('r') => {
                next_preview = Instant::now();
                if !app.refreshing && requests.try_send(()).is_ok() {
                    app.refreshing = true;
                    last_request = Instant::now();
                    app.notice = None;
                }
            }
            KeyCode::Char('i') if app.managed_id.is_some() => {
                app.terminal_input = true;
                app.show_conversation = false;
                app.show_help = false;
                app.show_details = false;
                owned_next_frame = Instant::now();
            }
            KeyCode::Enter => {
                if focus_query.is_some() {
                    app.notice = Some("A terminal focus request is still running.".into());
                    continue;
                }
                let selected = app.selected_session();
                let navigation = selected
                    .and_then(|s| ttybird::ui::navigation_session(s, &app.snapshots))
                    .cloned();
                if navigation.is_none() && selected.is_some() {
                    app.notice = Some(
                        "This child has no known parent terminal. Use c for its conversation."
                            .into(),
                    );
                }
                if let Some(session) = navigation {
                    if matches!(session.target, Some(Target::Managed { .. }))
                        && app.managed_id.is_some()
                    {
                        app.terminal_input = true;
                        app.show_conversation = false;
                        app.show_help = false;
                        app.show_details = false;
                        owned_next_frame = Instant::now();
                        continue;
                    }
                    if session.target.is_none() {
                        let is_local = app.snapshots.first().is_some_and(|snapshot| {
                            snapshot.host == session.host
                                && snapshot.sessions.iter().any(|row| {
                                    row.id == session.id && row.provider == session.provider
                                })
                        });
                        if session.pid.is_none() || session.process_started_at.is_none() {
                            app.notice = Some(
                                "Log-only history has no verified live session to focus.".into(),
                            );
                        } else if !is_local || !cfg!(target_os = "macos") {
                            app.notice = Some("No terminal mapping. Bind a terminal on the session's host with ttybird bind.".into());
                        } else if pane_query.is_none() {
                            app.notice = Some("Loading Ghostty panes… Esc cancels.".into());
                            let (tx, rx) = mpsc::sync_channel(1);
                            pane_query = Some(rx);
                            std::thread::spawn(move || {
                                let result =
                                    navigation::ghostty_terminals().map_err(|e| format!("{e:#}"));
                                let _ = tx.send((session, result));
                            });
                        }
                    } else if session.pid.is_none() || session.process_started_at.is_none() {
                        app.notice=Some("This is recent history; a live process has not been verified. Navigation is unavailable.".into());
                    } else {
                        let local_host = app.snapshots.first().map(|s| s.host.as_str());
                        let host = (local_host != Some(session.host.as_str()))
                            .then_some(session.host.clone());
                        if host.is_none() && matches!(session.target, Some(Target::Ghostty { .. }))
                        {
                            app.notice = Some("Focusing Ghostty…".into());
                            focus_query =
                                Some(focus_ghostty_from_dashboard(cli, dir, session.id, None));
                        } else {
                            destination = Some((session.id, host));
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    drop(requests);
    drop(ready);
    drop(preview_requests);
    drop(preview_ready);
    drop(terminal);
    drop(guard);
    owned_client.finish();
    // Do not abandon the bounded AppleScript helper or a pending binding
    // rollback when q, Ctrl-C or terminal hangup occurs during navigation.
    if let Some(receiver) = focus_query {
        let _ = receiver.recv();
    }
    if let Some((id, host)) = destination {
        focus(cli, dir, &id, &host)?;
    }
    Ok(())
}

fn should_open_tui(cli: &Cli, input_is_terminal: bool, output_is_terminal: bool) -> bool {
    !cli.json && !cli.plain && input_is_terminal && output_is_terminal
}

fn handoff_command(cli: &Cli, dir: &std::path::Path, command: &HandoffCommand) -> Result<()> {
    match command {
        HandoffCommand::Prepare {
            session,
            cwd,
            note,
            include_conversation,
        } => {
            let source = if let Some(id) = session {
                Some(find_session(&local_snapshot(cli, dir)?, id)?.clone())
            } else {
                None
            };
            if *include_conversation && source.is_none() {
                bail!("--include-conversation requires a verified local SESSION");
            }
            let cwd = cwd
                .clone()
                .or_else(|| {
                    source
                        .as_ref()
                        .and_then(|s| s.cwd.as_deref().map(PathBuf::from))
                })
                .unwrap_or(std::env::current_dir()?);
            let excerpt = if *include_conversation {
                Some(collect::recent_conversation(source.as_ref().unwrap())?)
            } else {
                None
            };
            let draft = ttybird::handoff::prepare(source.as_ref(), &cwd, note, excerpt.as_deref())?;
            let bundle = ttybird::handoff::save(dir, &draft)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"bundle":bundle,"destination":"codex","started":false})
                );
            } else {
                println!(
                    "Draft saved to {}
Edit draft.md, then review and start with: ttybird handoff start <bundle>",
                    clean(&bundle.display().to_string())
                );
            }
        }
        HandoffCommand::Start {
            bundle,
            yes,
            detach,
        } => {
            if !detach
                && (cli.json
                    || cli.plain
                    || !io::stdin().is_terminal()
                    || !io::stdout().is_terminal())
            {
                bail!("use --detach to start without an interactive dashboard");
            }
            let draft = ttybird::handoff::read(bundle)?;
            if !yes {
                if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                    bail!(
                        "review draft.md, then pass --yes to explicitly share this draft with Codex"
                    );
                }
                println!(
                    "Destination: Codex in {}

{}
",
                    clean(&draft.workspace.checkout_root.display().to_string()),
                    ttybird::handoff::clean(&draft.text)
                );
                print!("Share this draft and start a new Codex session? Type yes: ");
                io::stdout().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                if answer.trim() != "yes" {
                    println!("Cancelled; no session started.");
                    return Ok(());
                }
            }
            let session = ttybird::handoff::start(dir, &draft)?;
            if *detach {
                if cli.json {
                    println!("{}", serde_json::to_string(&session)?);
                } else {
                    println!("{}", session.id);
                }
            } else {
                interactive(cli, dir, false, Some(&session.id))?;
            }
        }
    }
    Ok(())
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let dir = cli
        .config_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(config::directory)?;
    match &cli.command {
        Some(Commands::Handoff { command }) => handoff_command(&cli, &dir, command)?,
        Some(Commands::Inbox { command }) => {
            let inbox = ttybird::attention::AttentionInbox::new(&dir);
            let now = chrono::Utc::now().timestamp();
            match command {
                Some(InboxCommand::Ack { id }) => {
                    inbox.acknowledge(id, now)?;
                }
                Some(InboxCommand::Snooze { id, minutes }) => {
                    inbox.snooze(id, now + (*minutes as i64) * 60)?;
                }
                None => {
                    refresh_with_inbox(&cli, &dir)?;
                }
            }
            let items = open_inbox(&dir)?;
            if cli.json {
                println!("{}", serde_json::to_string(&items)?);
            } else if items.is_empty() {
                println!(
                    "No open observed attention events. Claude hooks are required; unknown states are not inferred as requests."
                );
            } else {
                for item in items {
                    println!(
                        "{}  {}  {}  {}",
                        item.id,
                        item.kind.label(),
                        clean(item.title.as_deref().unwrap_or(&item.session_id)),
                        if item.is_snoozed(now) {
                            "snoozed"
                        } else if item.read_at.is_some() {
                            "read"
                        } else {
                            "new"
                        }
                    );
                }
            }
        }
        Some(Commands::SessionServer { id, name, command }) => {
            managed::serve(&dir, id, name.as_deref(), command)?
        }
        Some(Commands::Run {
            name,
            detach,
            command,
        }) => {
            if !detach
                && (cli.json
                    || cli.plain
                    || !io::stdin().is_terminal()
                    || !io::stdout().is_terminal())
            {
                bail!("run needs an interactive terminal; use --detach to start in the background");
            }
            let session = managed::launch(&dir, name.as_deref(), command)?;
            if *detach {
                if cli.json {
                    println!("{}", serde_json::to_string(&session)?);
                } else {
                    println!("{}", session.id);
                }
            } else if session.ended {
                println!(
                    "Session {} ended (exit {}).",
                    session.id,
                    session
                        .exit_code
                        .map_or_else(|| "signal".into(), |code| code.to_string())
                );
            } else {
                interactive(&cli, &dir, false, Some(&session.id))?;
            }
        }
        Some(Commands::Attach { session }) => {
            if cli.json || cli.plain {
                bail!("attach needs the interactive dashboard");
            }
            let sessions = managed::list(&dir)?;
            if !sessions.iter().any(|s| s.id == *session && !s.ended) {
                bail!("owned session is not running");
            }
            interactive(&cli, &dir, false, Some(session))?;
        }
        Some(Commands::Sessions) => {
            let sessions = managed::list(&dir)?;
            if cli.json {
                println!("{}", serde_json::to_string(&sessions)?);
            } else {
                for s in sessions {
                    println!(
                        "{}\t{}\t{}",
                        s.id,
                        if s.ended { "ended" } else { "running" },
                        clean(s.name.as_deref().unwrap_or(&s.program))
                    );
                }
            }
        }
        Some(Commands::Stop { session }) => {
            match managed::request(&dir, session, managed::Request::Stop)? {
                managed::Response::Ok => println!("Stopped {session}"),
                managed::Response::Error { message } => bail!("{message}"),
                _ => bail!("unexpected stop response"),
            }
        }
        Some(Commands::Tui) => {
            if cli.json || cli.plain {
                bail!("tui cannot be combined with --json or --plain");
            }
            interactive(&cli, &dir, false, None)?;
        }
        Some(Commands::Collect) => {
            println!("{}", serde_json::to_string(&local_snapshot(&cli, &dir)?)?);
        }
        Some(Commands::Hook) => {
            // Hook failures must never block or change the coding agent's decision.
            if let Err(e) = telemetry::record(&dir, io::stdin().lock()) {
                eprintln!("ttybird hook skipped: {e}");
            }
        }
        Some(Commands::Hooks) => {
            let exe = std::env::current_exe()?;
            let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
            let command = format!(
                "{} --config-dir {} hook",
                quote(&exe.to_string_lossy()),
                quote(&dir.to_string_lossy())
            );
            let mut hooks = serde_json::Map::new();
            for name in [
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PermissionRequest",
                "PostToolUse",
                "PostToolUseFailure",
                "Notification",
                "Stop",
                "SessionEnd",
            ] {
                hooks.insert(name.into(),serde_json::json!([{"hooks":[{"type":"command","command":command,"timeout":3}]}]));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({"hooks":hooks}))?
            );
        }
        Some(Commands::Hosts { command }) => {
            let _guard = if matches!(command, HostCommand::List) {
                None
            } else {
                Some(config::mutation_guard(&dir)?)
            };
            let mut config = config::read(&dir)?;
            match command {
                HostCommand::Add { name, binary } => {
                    remote::validate_host(name)?;
                    remote::validate_binary(binary)?;
                    config::add_host(&mut config, name.clone(), binary.clone())?;
                    config::write(&dir, &config)?;
                    println!(
                        "Registered {}. Collection will use SSH BatchMode.",
                        clean(name)
                    );
                }
                HostCommand::Remove { name } => {
                    config.hosts.retain(|h| &h.name != name);
                    config::write(&dir, &config)?;
                }
                HostCommand::List => println!("{}", serde_json::to_string_pretty(&config.hosts)?),
            }
        }
        Some(Commands::Terminals) => {
            let terminals = navigation::ghostty_terminals()?;
            if cli.json {
                println!("{}", serde_json::to_string(&terminals)?);
            } else {
                for t in terminals {
                    println!("{}  {}  {}", clean(&t.id), clean(&t.cwd), clean(&t.title));
                }
            }
        }
        Some(Commands::Bind {
            session,
            pid,
            ghostty,
            tmux,
            socket,
        }) => {
            let _guard = config::mutation_guard(&dir)?;
            let snapshot = local_snapshot(&cli, &dir)?;
            let s = find_session(&snapshot, session)?;
            let pid = pid
                .or(s.pid)
                .context("use --pid with a currently listed agent process")?;
            let process = snapshot
                .sessions
                .iter()
                .find(|p| p.pid == Some(pid))
                .context("PID is not a discovered coding-agent process")?;
            if process.provider != s.provider {
                bail!("process provider differs from session provider");
            }
            let started = identity(pid).context("process is no longer live")?;
            let target = if let Some(id) = ghostty {
                if !navigation::ghostty_terminals()?.iter().any(|t| &t.id == id) {
                    bail!("Ghostty terminal ID not found");
                }
                Target::Ghostty {
                    terminal_id: id.clone(),
                }
            } else {
                Target::Tmux {
                    socket: socket.clone(),
                    pane: tmux.clone().context("missing tmux pane")?,
                }
            };
            navigation::validate_target(&target)?;
            if let Some(target_tty) = navigation::target_tty(&target)? {
                let process_tty = process
                    .tty
                    .as_deref()
                    .context("process TTY unavailable; cannot bind this tmux pane")?;
                if process_tty.trim_start_matches("/dev/") != target_tty.trim_start_matches("/dev/")
                {
                    bail!("tmux pane TTY differs from the selected process TTY");
                }
            }
            let mut bindings = config::bindings(&dir)?;
            bindings.retain(|b| !(b.host == snapshot.host && b.session_id == *session));
            bindings.push(Binding {
                session_id: session.clone(),
                host: snapshot.host,
                pid,
                process_started_at: started,
                target,
            });
            config::save_bindings(&dir, &bindings)?;
            println!(
                "Bound exact session {}; future focus checks PID/start identity.",
                clean(session)
            );
        }
        Some(Commands::Unbind { session }) => {
            let _guard = config::mutation_guard(&dir)?;
            let mut bindings = config::bindings(&dir)?;
            bindings.retain(|b| &b.session_id != session);
            config::save_bindings(&dir, &bindings)?;
        }
        Some(Commands::Focus { session, host }) => focus(&cli, &dir, session, host)?,
        Some(Commands::Providers) => {
            let providers = provider_capabilities();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&providers)?);
            } else {
                println!("TOOL          PROCESS    SESSION LOGS    ACTIVITY");
                for provider in ttybird::model::Provider::ALL {
                    let logs = ttybird::providers::has_session_logs(*provider);
                    println!(
                        "{:<13} yes        {:<15} {}",
                        provider.label(),
                        if logs { "when available" } else { "not parsed" },
                        if logs {
                            "evidence-dependent"
                        } else {
                            "unknown"
                        }
                    );
                }
                println!(
                    "Exact executable/package entrypoints only. Process discovery does not prove active work. See docs/PROVIDERS.md for recognized launch forms."
                );
            }
        }
        Some(Commands::Doctor) => {
            let capabilities = serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"config_dir":dir,"registered_hosts":config::read(&dir)?.hosts,"providers":ttybird::model::Provider::ALL,"provider_capabilities":provider_capabilities(),"collection":"same-user processes and bounded recent log metadata","ghostty":if cfg!(target_os="macos") {"explicit AppleScript query/focus; macOS Automation permission may be required"} else {"macOS only"},"tmux":"default server and current TMUX socket; exact TTY association","terminal_preview":"opt-in TUI p; verified local tmux visible screen; libghostty-vt 0.2.1; no input or persistence","claude_hooks":"opt-in: ttybird hooks prints configuration fragment","codex_app_server":"not connected; log metadata adapter only","superlogical":"adapter not implemented: no verified public integration API","managed_terminals":"explicit run/attach/stop; owned PTY with in-memory libghostty screen; input only in INPUT mode","safety":"discovery stays read-only; launch, input and stop apply only to explicitly owned sessions; no automatic approvals"});
            println!("{}", serde_json::to_string_pretty(&capabilities)?);
        }
        Some(Commands::List | Commands::NeedsMe) | None => {
            let needs_me = matches!(cli.command, Some(Commands::NeedsMe));
            if should_open_tui(&cli, io::stdin().is_terminal(), io::stdout().is_terminal()) {
                return interactive(&cli, &dir, needs_me, None);
            }
            let running = Arc::new(AtomicBool::new(true));
            let flag = running.clone();
            ctrlc::set_handler(move || flag.store(false, Ordering::SeqCst))?;
            while running.load(Ordering::SeqCst) {
                let snapshots = if cli.notify {
                    refresh_with_inbox(&cli, &dir)?.0
                } else {
                    all_snapshots(&cli, &dir)?
                };
                if cli.watch && !cli.json && io::stdout().is_terminal() {
                    print!("\x1b[2J\x1b[H");
                }
                render(&snapshots, needs_me, cli.live_only, cli.json)?;
                io::stdout().flush()?;
                if !cli.watch {
                    break;
                }
                for _ in 0..cli.interval * 10 {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        // Broken pipes are normal when a user pipes a snapshot into head.
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        {
            return;
        }
        // stderr can itself be a revoked PTY. Error reporting must not panic.
        let _ = writeln!(io::stderr(), "ttybird: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dashboard_defaults_only_for_interactive_human_output() {
        let cli = Cli::try_parse_from(["ttybird", "--local"]).unwrap();
        assert!(should_open_tui(&cli, true, true));
        assert!(!should_open_tui(&cli, false, true));
        assert!(!should_open_tui(&cli, true, false));
        let plain = Cli::try_parse_from(["ttybird", "--plain"]).unwrap();
        assert!(!should_open_tui(&plain, true, true));
        let json = Cli::try_parse_from(["ttybird", "--json"]).unwrap();
        assert!(!should_open_tui(&json, true, true));
    }
    fn fixture_session() -> Session {
        serde_json::from_value(serde_json::json!({"id":"session","provider":"codex","parent_id":null,"host":"local","pid":20,"process_started_at":200,"tty":"/dev/pts/2","cwd":null,"model":null,"activity":"unknown","confidence":"observed","evidence":"fixture","updated_at":null,"target":null})).unwrap()
    }

    #[test]
    fn failed_focus_restores_only_its_own_binding() {
        let dir = tempfile::tempdir().unwrap();
        let saved = Binding {
            session_id: "session".into(),
            host: "local".into(),
            pid: 20,
            process_started_at: 200,
            target: Target::Ghostty {
                terminal_id: "11111111-1111-1111-1111-111111111111".into(),
            },
        };
        let mut old = saved.clone();
        old.target = Target::Ghostty {
            terminal_id: "22222222-2222-2222-2222-222222222222".into(),
        };
        config::save_bindings(dir.path(), std::slice::from_ref(&saved)).unwrap();
        rollback_picker_binding(
            dir.path(),
            &PendingBinding {
                saved: saved.clone(),
                previous: Some(old.clone()),
            },
        )
        .unwrap();
        assert_eq!(config::bindings(dir.path()).unwrap()[0].target, old.target);
        // A concurrent writer's new target must not be overwritten by compensation.
        rollback_picker_binding(
            dir.path(),
            &PendingBinding {
                saved: saved.clone(),
                previous: None,
            },
        )
        .unwrap();
        assert_eq!(config::bindings(dir.path()).unwrap()[0].target, old.target);
        config::save_bindings(dir.path(), std::slice::from_ref(&saved)).unwrap();
        rollback_picker_binding(
            dir.path(),
            &PendingBinding {
                saved,
                previous: None,
            },
        )
        .unwrap();
        assert!(config::bindings(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn pane_choice_rejects_changed_session_identity() {
        let expected = fixture_session();
        assert!(verify_picker_session(&expected, &expected, Some(200)).is_ok());
        assert!(verify_picker_session(&expected, &expected, Some(201)).is_err());
        assert!(verify_picker_session(&expected, &expected, None).is_err());
        for field in ["host", "id", "provider", "pid", "start", "tty"] {
            let mut changed = expected.clone();
            match field {
                "host" => changed.host = "remote".into(),
                "id" => changed.id = "other".into(),
                "provider" => changed.provider = ttybird::model::Provider::Claude,
                "pid" => changed.pid = Some(21),
                "start" => changed.process_started_at = Some(201),
                "tty" => changed.tty = Some("/dev/pts/3".into()),
                _ => unreachable!(),
            }
            assert!(
                verify_picker_session(&expected, &changed, Some(200)).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn saved_binding_cannot_override_a_resumed_live_session() {
        let mut session = fixture_session();
        let binding = Binding {
            session_id: "session".into(),
            host: "local".into(),
            pid: 10,
            process_started_at: 100,
            target: Target::Tmux {
                socket: None,
                pane: "%1".into(),
            },
        };
        assert!(!apply_binding(&mut session, &binding, Some(100)));
        assert_eq!(session.pid, Some(20));
        assert!(session.target.is_none());
        session.pid = None;
        session.process_started_at = None;
        assert!(!apply_binding(&mut session, &binding, None));
        assert!(!apply_binding(&mut session, &binding, Some(100)));
        assert!(session.pid.is_none());
        assert!(session.target.is_none());
        session.pid = Some(10);
        session.process_started_at = Some(100);
        assert!(apply_binding(&mut session, &binding, Some(100)));
    }
    #[test]
    fn strip_terminal_controls() {
        assert_eq!(clean("x\x1b[2Jy\nz"), "x [2Jy z");
    }
    #[test]
    fn bind_requires_one_target() {
        assert!(Cli::try_parse_from(["ttybird", "bind", "id"]).is_err());
        assert!(
            Cli::try_parse_from(["ttybird", "bind", "id", "--tmux", "%1", "--ghostty", "x"])
                .is_err()
        );
    }
}
