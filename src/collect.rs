use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde_json::Value;
use sysinfo::{
    Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind, get_current_pid,
};
use walkdir::WalkDir;

use crate::{
    model::{
        Activity, ActivityObservation, Confidence, Provider, Session, SessionInsights, Snapshot,
        TokenUsage, TokenUsageScope,
    },
    providers,
    remote::run_bounded,
};

const HEAD_BYTES: u64 = 256 * 1024;
const TAIL_BYTES: u64 = 512 * 1024;
const MODEL_LOOKBACK_BYTES: u64 = 4 * 1024 * 1024;
const INDEX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WALK_ENTRIES: usize = 50_000;
const LIVE_STATE_FRESHNESS_SECONDS: i64 = 5 * 60;
const RECENT_CONVERSATION_MESSAGES: usize = 3;
const RECENT_MESSAGE_CHARS: usize = 2_000;

#[derive(Debug, Clone)]
pub struct CollectOptions {
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    pub recent_minutes: u64,
    pub max_logs: usize,
}

impl Default for CollectOptions {
    fn default() -> Self {
        let home = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
        Self {
            codex_home: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            claude_home: std::env::var_os("CLAUDE_CONFIG_DIR")
                .or_else(|| std::env::var_os("CLAUDE_HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
            recent_minutes: 24 * 60,
            max_logs: 100,
        }
    }
}

#[derive(Debug, Clone)]
struct LiveProcess {
    provider: Provider,
    pid: u32,
    started_at: u64,
    tty: Option<String>,
    cwd: Option<String>,
    headless: bool,
    open_files: HashSet<PathBuf>,
    open_files_complete: bool,
}

#[derive(Debug)]
struct LogCandidate {
    provider: Provider,
    path: PathBuf,
    modified_at: i64,
    matched_process: Option<usize>,
}

#[derive(Debug, Default)]
struct LogScan {
    logs: Vec<LogCandidate>,
    errors: usize,
    visited_entries: usize,
    truncated: bool,
}

#[derive(Debug, Default)]
struct ParsedLog {
    id: Option<String>,
    parent_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    state: LogState,
    state_at: Option<i64>,
    omit_unidentified_child: bool,
    title: Option<String>,
    usage: Option<TokenUsage>,
}

#[derive(Debug, Default)]
struct SampledLines {
    lines: Vec<Vec<u8>>,
    /// When present, lifecycle state from lines before this index is not
    /// carried over the unobserved middle of the file.
    tail_after_gap: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum LogState {
    Started,
    Completed,
    Aborted,
    ClaudeUser,
    ClaudeStreaming,
    ClaudeStopped,
    #[default]
    Unknown,
}

pub fn collect(options: &CollectOptions) -> Result<Snapshot> {
    let host = System::host_name().unwrap_or_else(|| "localhost".to_owned());
    let mut snapshot = Snapshot::new(host.clone());
    let (processes, process_warning) = live_processes();
    if let Some(warning) = process_warning {
        snapshot.warnings.push(warning);
    }

    let process_file_map = unique_process_file_map(&processes);

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            options.recent_minutes.saturating_mul(60),
        ))
        .unwrap_or(UNIX_EPOCH);
    let mut scan = LogScan::default();
    gather_logs(
        &options.codex_home.join("sessions"),
        Provider::Codex,
        cutoff,
        &process_file_map,
        &mut scan,
    );
    gather_logs(
        &options.claude_home.join("projects"),
        Provider::Claude,
        cutoff,
        &process_file_map,
        &mut scan,
    );
    if scan.errors > 0 {
        snapshot.warnings.push(format!(
            "could not inspect {} log entries; collection continued",
            scan.errors
        ));
    }
    if scan.truncated {
        snapshot.warnings.push(format!(
            "log scan stopped after {MAX_WALK_ENTRIES} filesystem entries"
        ));
    }

    // A descriptor-proven live log wins over an unrelated newer historical log.
    scan.logs
        .sort_by_key(|log| (log.matched_process.is_some(), log.modified_at));
    scan.logs.reverse();
    scan.logs.truncate(options.max_logs);

    let codex_titles = match load_codex_titles(&options.codex_home.join("session_index.jsonl")) {
        Ok(titles) => titles,
        Err(_) => {
            snapshot
                .warnings
                .push("could not read the bounded Codex title index".to_owned());
            HashMap::new()
        }
    };

    let mut represented_processes = HashSet::new();
    let mut parse_errors = 0usize;
    let mut omitted_claude_children = 0usize;
    for log in scan.logs {
        let parsed = match parse_log(&log.path, &log.provider) {
            Ok(parsed) => parsed,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        if parsed.omit_unidentified_child {
            omitted_claude_children += 1;
            continue;
        }
        let Some(id) = parsed.id else {
            continue;
        };
        let process = log.matched_process.and_then(|index| processes.get(index));
        if let Some(index) = log.matched_process {
            represented_processes.insert(index);
        }
        let is_live = process.is_some();
        let lifecycle_is_fresh = parsed.state_at.is_some_and(|timestamp| {
            (0..=LIVE_STATE_FRESHNESS_SECONDS)
                .contains(&snapshot.collected_at.saturating_sub(timestamp))
        });
        let insights = SessionInsights {
            activity_observation: activity_observation(log.provider, parsed.state, parsed.state_at),
            workspace: None,
            sharing: None,
            title: if log.provider == Provider::Codex {
                codex_titles.get(&id).cloned().or(parsed.title)
            } else {
                parsed.title
            },
            usage: parsed.usage,
            log_path: Some(log.path.clone()),
        };
        snapshot.sessions.push(Session {
            id,
            provider: log.provider,
            parent_id: parsed.parent_id,
            host: host.clone(),
            pid: process.map(|value| value.pid),
            process_started_at: process.map(|value| value.started_at),
            tty: process.and_then(|value| value.tty.clone()),
            cwd: parsed.cwd.or_else(|| process.and_then(|value| value.cwd.clone())),
            model: parsed.model,
            activity: activity(parsed.state, is_live, lifecycle_is_fresh),
            confidence: Confidence::Inferred,
            evidence: if process.is_some_and(|value| value.headless) {
                "same-user headless process and log matched by a unique writable descriptor; inherited terminal target suppressed; activity inferred from the latest sampled lifecycle event".to_owned()
            } else if is_live {
                "same-user process and log matched by a unique writable descriptor; activity inferred from the latest sampled lifecycle event".to_owned()
            } else {
                "recent historical log; no matching live process; activity inferred from sampled lifecycle events".to_owned()
            },
            updated_at: Some(log.modified_at),
            target: None,
            insights,
        });
    }
    if parse_errors > 0 {
        snapshot.warnings.push(format!(
            "could not read {parse_errors} bounded log samples; collection continued"
        ));
    }
    if omitted_claude_children > 0 {
        snapshot.warnings.push(format!(
            "omitted {omitted_claude_children} Claude sidechain logs without an agentId"
        ));
    }

    for (index, process) in processes.into_iter().enumerate() {
        if represented_processes.contains(&index) {
            continue;
        }
        let provider_name = process.provider.as_str();
        snapshot.sessions.push(Session {
            id: format!("{provider_name}-pid-{}-{}", process.pid, process.started_at),
            provider: process.provider,
            parent_id: None,
            host: host.clone(),
            pid: Some(process.pid),
            process_started_at: Some(process.started_at),
            tty: process.tty,
            cwd: process.cwd,
            model: None,
            activity: Activity::Unknown,
            confidence: Confidence::Observed,
            evidence: if process.headless {
                format!(
                    "same-user live {provider_name} headless process; inherited terminal target suppressed; session metadata unavailable"
                )
            } else {
                format!("same-user live {provider_name} process; session metadata unavailable")
            },
            updated_at: None,
            target: None,
            insights: SessionInsights::default(),
        });
    }

    revalidate_processes(&mut snapshot);
    snapshot.sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(snapshot)
}

/// Read a small plaintext conversation excerpt only after revalidating the
/// exact live process and its uniquely owned writable transcript descriptor.
/// The returned text is intended for an explicit local UI action and is never
/// stored in [`Session`] or serialized in snapshots.
pub fn recent_conversation(session: &Session) -> Result<String> {
    let Some(pid) = session.pid else {
        anyhow::bail!("conversation preview requires a live session");
    };
    let Some(started_at) = session.process_started_at else {
        anyhow::bail!("conversation preview requires a verified process identity");
    };
    let Some(stored_path) = session.insights.log_path.as_ref() else {
        anyhow::bail!("conversation transcript is unavailable");
    };
    if !matches!(session.provider, Provider::Codex | Provider::Claude) {
        anyhow::bail!("conversation preview is unavailable for this provider");
    }

    let path =
        fs::canonicalize(stored_path).with_context(|| "revalidate the conversation transcript")?;
    let (processes, _) = live_processes();
    verify_conversation_owner(&processes, session, &path)?;

    // Keep one descriptor for identity and content so a path replacement cannot
    // make the metadata check and bounded read observe different files.
    let mut file = File::open(&path).with_context(|| "open the conversation transcript")?;
    if !open_file_matches_path(&file, &path)? {
        anyhow::bail!("conversation transcript path changed before the read");
    }
    if session_id_from_head(&mut file, session.provider)?.as_deref() != Some(session.id.as_str()) {
        anyhow::bail!("conversation transcript identity changed");
    }

    let lines = bounded_tail_lines(&mut file, TAIL_BYTES)?;
    if !open_file_matches_path(&file, &path)? {
        anyhow::bail!("conversation transcript path changed during the read");
    }
    let refreshed_path = fs::canonicalize(stored_path)
        .with_context(|| "revalidate the conversation transcript after reading")?;
    if refreshed_path != path {
        anyhow::bail!("conversation transcript path changed during the read");
    }
    let (refreshed_processes, _) = live_processes();
    verify_conversation_owner(&refreshed_processes, session, &path)?;
    if !open_file_matches_path(&file, &path)? {
        anyhow::bail!("conversation transcript path changed during revalidation");
    }
    if process_identity(pid) != Some(started_at) {
        anyhow::bail!("conversation process identity changed during the read");
    }
    let messages = recent_plaintext_messages(session.provider, lines);
    if messages.is_empty() {
        anyhow::bail!("no recent plaintext conversation is available");
    }
    Ok(messages
        .into_iter()
        .map(|(role, text)| format!("{role}: {text}"))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn verify_conversation_owner(
    processes: &[LiveProcess],
    session: &Session,
    path: &Path,
) -> Result<()> {
    let pid = session
        .pid
        .context("conversation preview requires a live session")?;
    let started_at = session
        .process_started_at
        .context("conversation preview requires a verified process identity")?;
    if processes
        .iter()
        .any(|process| process.provider == session.provider && !process.open_files_complete)
    {
        anyhow::bail!("conversation ownership could not be fully revalidated");
    }
    let matches: Vec<_> = processes
        .iter()
        .enumerate()
        .filter(|(_, process)| {
            process.pid == pid
                && process.started_at == started_at
                && process.provider == session.provider
        })
        .collect();
    if matches.len() != 1 {
        anyhow::bail!("conversation process identity changed");
    }
    let owners = unique_process_file_map(processes);
    if owners.get(path) != Some(&(matches[0].0, session.provider)) {
        anyhow::bail!("conversation transcript is not uniquely owned by the session");
    }
    Ok(())
}

#[cfg(unix)]
fn open_file_matches_path(file: &File, path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let open = file.metadata()?;
    let current = fs::metadata(path)?;
    Ok(open.dev() == current.dev() && open.ino() == current.ino())
}

#[cfg(not(unix))]
fn open_file_matches_path(_file: &File, _path: &Path) -> Result<bool> {
    // Windows does not expose a stable std file identity. The single open
    // descriptor plus post-read canonical path/ownership checks still apply.
    Ok(true)
}

fn session_id_from_head(file: &mut File, provider: Provider) -> Result<Option<String>> {
    let length = file.metadata()?.len();
    let head_end = length.min(HEAD_BYTES);
    let lines = read_complete_range(file, 0, head_end, length)?;
    Ok(match provider {
        Provider::Codex => lines.into_iter().find_map(|line| {
            let value = serde_json::from_slice::<Value>(&line).ok()?;
            (value.get("type").and_then(Value::as_str) == Some("session_meta"))
                .then(|| {
                    value.get("payload").and_then(|payload| {
                        string_at(payload, &["id"]).or_else(|| string_at(payload, &["session_id"]))
                    })
                })
                .flatten()
        }),
        Provider::Claude => {
            let mut session_id = None;
            let mut agent_id = None;
            let mut is_sidechain = false;
            for line in lines {
                let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                session_id = string_at(&value, &["sessionId"]).or(session_id);
                agent_id = string_at(&value, &["agentId"]).or(agent_id);
                is_sidechain |= value
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            match (session_id, agent_id, is_sidechain) {
                (Some(parent_id), Some(agent_id), _) => {
                    Some(format!("{parent_id}:agent:{agent_id}"))
                }
                (Some(_), None, true) => None,
                (session_id, None, false) => session_id,
                (None, Some(_), _) => None,
                (None, None, true) => None,
            }
        }
        _ => None,
    })
}

/// Remove process-only rows that died during sampling and detach historical logs.
fn revalidate_processes(snapshot: &mut Snapshot) {
    let pids: Vec<_> = snapshot
        .sessions
        .iter()
        .filter_map(|s| s.pid)
        .collect::<HashSet<_>>()
        .into_iter()
        .map(Pid::from_u32)
        .collect();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    snapshot.sessions.retain_mut(|s| {
        let Some(pid) = s.pid else {
            return true;
        };
        let valid = system.process(Pid::from_u32(pid)).is_some_and(|p| {
            process_is_live(p.status()) && Some(p.start_time()) == s.process_started_at
        });
        if valid {
            return true;
        }
        if s.id
            == format!(
                "{}-pid-{}-{}",
                s.provider.as_str(),
                pid,
                s.process_started_at.unwrap_or(0)
            )
        {
            return false;
        }
        s.pid = None;
        s.process_started_at = None;
        s.tty = None;
        s.target = None;
        s.activity = Activity::Unknown;
        s.evidence = "historical log; process exited or identity changed during collection".into();
        true
    });
}

/// Returns the process start time in seconds since the Unix epoch. Callers must
/// compare it together with the PID before using a previously stored binding.
pub fn process_identity(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system
        .process(pid)
        .filter(|p| process_is_live(p.status()) && p.start_time() > 0)
        .map(|process| process.start_time())
}

pub(crate) fn process_is_live(status: ProcessStatus) -> bool {
    !matches!(status, ProcessStatus::Zombie | ProcessStatus::Dead)
}

fn live_processes() -> (Vec<LiveProcess>, Option<String>) {
    let Ok(current_pid) = get_current_pid() else {
        return (
            Vec::new(),
            Some("could not establish the collector process identity".to_owned()),
        );
    };
    let refresh = ProcessRefreshKind::nothing()
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cwd(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .without_tasks();
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    let Some(current_uid) = system
        .process(current_pid)
        .and_then(|process| process.user_id())
        .cloned()
    else {
        return (
            Vec::new(),
            Some("could not establish the current user; live processes were omitted".to_owned()),
        );
    };

    let mut result = Vec::new();
    let mut descriptor_failures = 0usize;
    for (pid, process) in system.processes() {
        if process.user_id() != Some(&current_uid)
            || !process_is_live(process.status())
            || process.start_time() == 0
        {
            continue;
        }
        let Some(provider) = providers::classify(process.name(), process.exe(), process.cmd())
        else {
            continue;
        };
        let pid = pid.as_u32();
        let headless = providers::is_headless(provider, process.cmd());
        let (open_files, open_files_complete) = if providers::has_session_logs(provider) {
            match open_file_paths(pid) {
                Ok(paths) => (paths, true),
                Err(_) => {
                    descriptor_failures += 1;
                    (HashSet::new(), false)
                }
            }
        } else {
            (HashSet::new(), true)
        };
        result.push(LiveProcess {
            provider,
            pid,
            started_at: process.start_time(),
            tty: (!headless).then(|| process_tty(pid)).flatten(),
            cwd: process
                .cwd()
                .map(|path| path.to_string_lossy().into_owned()),
            headless,
            open_files,
            open_files_complete,
        });
    }
    let warning = (descriptor_failures > 0).then(|| {
        format!(
            "could not inspect open files for {descriptor_failures} agent processes; their logs were not associated"
        )
    });
    (result, warning)
}

fn unique_process_file_map(processes: &[LiveProcess]) -> HashMap<PathBuf, (usize, Provider)> {
    let mut owners = HashMap::<PathBuf, Vec<(usize, Provider)>>::new();
    for (index, process) in processes.iter().enumerate() {
        for path in &process.open_files {
            owners
                .entry(path.clone())
                .or_default()
                .push((index, process.provider));
        }
    }
    owners
        .into_iter()
        .filter_map(|(path, owners)| (owners.len() == 1).then(|| (path, owners[0])))
        .collect()
}

fn process_tty(pid: u32) -> Option<String> {
    let output = run_bounded(
        "ps",
        &["-o".into(), "tty=".into(), "-p".into(), pid.to_string()],
        Duration::from_secs(2),
        256,
    )
    .ok()?;
    let value = String::from_utf8(output).ok()?;
    let value = normalized_tty(&value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if !fs::metadata(Path::new("/dev").join(&value))
            .ok()?
            .file_type()
            .is_char_device()
        {
            return None;
        }
    }
    Some(value)
}

fn normalized_tty(value: &str) -> Option<String> {
    let value = value.trim().strip_prefix("/dev/").unwrap_or(value.trim());
    if value.is_empty()
        || matches!(value, "?" | "??" | "-")
        || value.starts_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(target_os = "linux")]
fn open_file_paths(pid: u32) -> Result<HashSet<PathBuf>> {
    let mut paths = HashSet::new();
    for entry in fs::read_dir(format!("/proc/{pid}/fd"))? {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let Some(fd) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(fdinfo) = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else {
            continue;
        };
        if !linux_fdinfo_is_writable(&fdinfo) {
            continue;
        }
        if target.is_absolute()
            && let Ok(path) = fs::canonicalize(target)
        {
            paths.insert(path);
        }
    }
    Ok(paths)
}

#[cfg(target_os = "linux")]
fn linux_fdinfo_is_writable(fdinfo: &str) -> bool {
    fdinfo.lines().any(|line| {
        line.strip_prefix("flags:")
            .and_then(|flags| u64::from_str_radix(flags.trim(), 8).ok())
            .is_some_and(|flags| flags & 3 != 0)
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_file_paths(pid: u32) -> Result<HashSet<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;

    let output = run_bounded(
        "lsof",
        &[
            "-nP".into(),
            "-a".into(),
            "-p".into(),
            pid.to_string(),
            "-Ffan".into(),
        ],
        Duration::from_secs(2),
        1024 * 1024,
    )
    .context("run bounded lsof")?;
    let mut paths = HashSet::new();
    for path in lsof_writable_names(&output) {
        let path = Path::new(OsStr::from_bytes(path));
        if path.is_absolute()
            && let Ok(path) = fs::canonicalize(path)
        {
            paths.insert(path);
        }
    }
    Ok(paths)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn lsof_writable_names(output: &[u8]) -> Vec<&[u8]> {
    let mut writable = false;
    let mut paths = Vec::new();
    for line in output.split(|byte| *byte == b'\n') {
        match line.first() {
            Some(b'f') => writable = false,
            Some(b'a') => writable = matches!(line.get(1), Some(b'w' | b'u')),
            Some(b'n') if writable && line.len() > 1 => paths.push(&line[1..]),
            _ => {}
        }
    }
    paths
}

#[cfg(not(unix))]
fn open_file_paths(_pid: u32) -> Result<HashSet<PathBuf>> {
    anyhow::bail!("open-file matching is unavailable on this platform")
}

fn gather_logs(
    root: &Path,
    provider: Provider,
    cutoff: SystemTime,
    process_file_map: &HashMap<PathBuf, (usize, Provider)>,
    scan: &mut LogScan,
) {
    if !root.is_dir() || scan.truncated {
        return;
    }
    for entry in WalkDir::new(root).follow_links(false) {
        if scan.visited_entries >= MAX_WALK_ENTRIES {
            scan.truncated = true;
            break;
        }
        scan.visited_entries += 1;
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                scan.errors += 1;
                continue;
            }
        };
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(OsStr::to_str) != Some("jsonl")
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            scan.errors += 1;
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            scan.errors += 1;
            continue;
        };
        let Ok(canonical_path) = fs::canonicalize(entry.path()) else {
            scan.errors += 1;
            continue;
        };
        let modified_at = modified
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_secs().min(i64::MAX as u64) as i64)
            .unwrap_or(0);
        let matched_process = process_file_map
            .get(&canonical_path)
            .filter(|(_, owner_provider)| owner_provider == &provider)
            .map(|(index, _)| *index);
        if modified < cutoff && matched_process.is_none() {
            continue;
        }
        scan.logs.push(LogCandidate {
            provider,
            path: entry.into_path(),
            matched_process,
            modified_at,
        });
    }
}

fn parse_log(path: &Path, provider: &Provider) -> Result<ParsedLog> {
    let sample = bounded_lines(path)?;
    Ok(match provider {
        Provider::Codex => {
            let needs_model_lookback = sample.tail_after_gap.is_some()
                && !sample
                    .lines
                    .get(sample.tail_after_gap.unwrap_or_default()..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| codex_model(line).is_some());
            let mut parsed = parse_codex_sample(sample);
            if needs_model_lookback {
                parsed.model = latest_codex_model(path)?;
            }
            parsed
        }
        Provider::Claude => parse_claude_sample(sample),
        _ => anyhow::bail!("{} session logs are unsupported", provider.as_str()),
    })
}

fn latest_codex_model(path: &Path) -> Result<Option<String>> {
    let mut file = File::open(path).with_context(|| "open a session log for model lookback")?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(MODEL_LOOKBACK_BYTES);
    let lines = read_complete_range(&mut file, start, length - start, length)?;
    Ok(lines.iter().rev().find_map(|line| codex_model(line)))
}

fn codex_model(line: &[u8]) -> Option<String> {
    const TURN_CONTEXT: &[u8] = b"\"turn_context\"";
    if !line
        .windows(TURN_CONTEXT.len())
        .any(|window| window == TURN_CONTEXT)
    {
        return None;
    }
    let value = serde_json::from_slice::<Value>(line).ok()?;
    (value.get("type").and_then(Value::as_str) == Some("turn_context"))
        .then(|| {
            value
                .get("payload")
                .and_then(|payload| string_at(payload, &["model"]))
        })
        .flatten()
}

fn load_codex_titles(path: &Path) -> Result<HashMap<String, String>> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let mut file = File::open(path).with_context(|| "open the Codex title index")?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(INDEX_BYTES);
    let lines = read_complete_range(&mut file, start, length - start, length)?;
    let mut titles = HashMap::new();
    for line in lines {
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let Some(id) = string_at(&value, &["id"]) else {
            continue;
        };
        if let Some(title) = title_at(&value) {
            titles.insert(id, title);
        }
    }
    Ok(titles)
}

fn title_at(value: &Value) -> Option<String> {
    ["custom_title", "customTitle", "thread_name", "title"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .and_then(bounded_title)
}

fn bounded_title(value: &str) -> Option<String> {
    let title: String = value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect();
    (!title.is_empty()).then_some(title)
}

fn codex_total_usage(value: &Value) -> Option<TokenUsage> {
    let total = value
        .get("payload")?
        .get("info")?
        .get("total_token_usage")?;
    let input_tokens = u64_at(total, "input_tokens")?;
    let cached_input_tokens = u64_at(total, "cached_input_tokens");
    let output_tokens = u64_at(total, "output_tokens")?;
    let reasoning_output_tokens = u64_at(total, "reasoning_output_tokens");
    let total_tokens = u64_at(total, "total_tokens")?;
    if cached_input_tokens.is_some_and(|cached| cached > input_tokens)
        || reasoning_output_tokens.is_some_and(|reasoning| reasoning > output_tokens)
        || input_tokens.checked_add(output_tokens) != Some(total_tokens)
    {
        return None;
    }
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        scope: TokenUsageScope::Total,
    })
}

fn u64_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn bounded_lines(path: &Path) -> Result<SampledLines> {
    let mut file = File::open(path).with_context(|| "open a session log")?;
    let length = file.metadata()?.len();
    let head_end = length.min(HEAD_BYTES);
    let mut lines = read_complete_range(&mut file, 0, head_end, length)?;
    let tail_start = length.saturating_sub(TAIL_BYTES).max(head_end);
    let mut tail_after_gap = None;
    if tail_start < length {
        if tail_start > head_end {
            tail_after_gap = Some(lines.len());
        }
        lines.extend(read_complete_range(
            &mut file,
            tail_start,
            length - tail_start,
            length,
        )?);
    }
    Ok(SampledLines {
        lines,
        tail_after_gap,
    })
}

fn bounded_tail_lines(file: &mut File, limit: u64) -> Result<Vec<Vec<u8>>> {
    let length = file.metadata()?.len();
    let start = length.saturating_sub(limit);
    read_complete_range(file, start, length - start, length)
}

fn recent_plaintext_messages(
    provider: Provider,
    lines: Vec<Vec<u8>>,
) -> Vec<(&'static str, String)> {
    let mut messages = Vec::new();
    for line in lines {
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let message = match provider {
            Provider::Codex => codex_plaintext_message(&value),
            Provider::Claude => claude_plaintext_message(&value),
            _ => None,
        };
        if let Some(message) = message {
            messages.push(message);
            if messages.len() > RECENT_CONVERSATION_MESSAGES {
                messages.remove(0);
            }
        }
    }
    messages
}

fn codex_plaintext_message(value: &Value) -> Option<(&'static str, String)> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let (role, block_type) = match payload.get("role").and_then(Value::as_str) {
        Some("user") => ("User", "input_text"),
        Some("assistant") => ("Assistant", "output_text"),
        _ => return None,
    };
    plaintext_content(payload.get("content")?, block_type).map(|text| (role, text))
}

fn claude_plaintext_message(value: &Value) -> Option<(&'static str, String)> {
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let role = match value.get("type").and_then(Value::as_str) {
        Some("user") => "User",
        Some("assistant") => "Assistant",
        _ => return None,
    };
    let message = value.get("message")?;
    let message_role = message.get("role").and_then(Value::as_str)?;
    if !matches!(
        (role, message_role),
        ("User", "user") | ("Assistant", "assistant")
    ) {
        return None;
    }
    plaintext_content(message.get("content")?, "text").map(|text| (role, text))
}

fn plaintext_content(value: &Value, allowed_block_type: &str) -> Option<String> {
    let pieces: Vec<&str> = match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some(allowed_block_type))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => return None,
    };
    if pieces.is_empty() {
        return None;
    }
    let joined = pieces.join("\n");
    let text: String = joined
        .trim()
        .chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            character if character.is_control() => ' ',
            character => character,
        })
        .take(RECENT_MESSAGE_CHARS)
        .collect();
    (!text.is_empty()).then_some(text)
}

fn read_complete_range(
    file: &mut File,
    start: u64,
    length: u64,
    file_length: u64,
) -> Result<Vec<Vec<u8>>> {
    let starts_at_line_boundary = if start == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0_u8; 1];
        file.read_exact(&mut previous)?;
        previous[0] == b'\n'
    };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    let mut begin = 0usize;
    if !starts_at_line_boundary {
        let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        begin = newline + 1;
    }
    let mut end = bytes.len();
    if start + length < file_length || bytes.last() != Some(&b'\n') {
        let Some(newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        end = newline + 1;
    }
    Ok(bytes[begin..end]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

#[cfg(test)]
fn parse_codex(lines: Vec<Vec<u8>>) -> ParsedLog {
    parse_codex_sample(SampledLines {
        lines,
        tail_after_gap: None,
    })
}

fn parse_codex_sample(sample: SampledLines) -> ParsedLog {
    let mut parsed = ParsedLog::default();
    for (index, line) in sample.lines.into_iter().enumerate() {
        if sample.tail_after_gap == Some(index) {
            parsed.state = LogState::Unknown;
            parsed.state_at = None;
            // Codex usage records are cumulative snapshots. A snapshot from
            // before an unread gap is not the final session total.
            parsed.usage = None;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                parsed.id = string_at(payload, &["id"])
                    .or_else(|| string_at(payload, &["session_id"]))
                    .or(parsed.id);
                parsed.cwd = string_at(payload, &["cwd"]).or(parsed.cwd);
                parsed.title = title_at(payload).or(parsed.title);
                parsed.parent_id = string_at(payload, &["parent_thread_id"])
                    .or_else(|| {
                        string_at(
                            payload,
                            &["source", "subagent", "thread_spawn", "parent_thread_id"],
                        )
                    })
                    .or_else(|| {
                        string_at(
                            payload,
                            &["source", "subagent", "spawn", "parent_thread_id"],
                        )
                    })
                    .or(parsed.parent_id);
            }
            Some("turn_context") => {
                parsed.model = value
                    .get("payload")
                    .and_then(|payload| string_at(payload, &["model"]))
                    .or(parsed.model);
            }
            Some("event_msg") => {
                let event_type = value
                    .get("payload")
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str);
                let state = match event_type {
                    Some("task_started") => Some(LogState::Started),
                    Some("task_complete") => Some(LogState::Completed),
                    Some("turn_aborted") => Some(LogState::Aborted),
                    _ => None,
                };
                if let Some(state) = state {
                    parsed.state = state;
                    parsed.state_at = record_timestamp(&value);
                } else if matches!(event_type, Some("item_completed" | "token_count"))
                    && parsed.state == LogState::Started
                    && let Some(observed_at) = record_timestamp(&value)
                    && parsed
                        .state_at
                        .is_none_or(|previous| observed_at >= previous)
                {
                    // Refresh only an explicitly sampled start. Asynchronous
                    // progress can arrive after task_complete, so it cannot
                    // establish a turn after an unread gap or unknown state.
                    parsed.state_at = Some(observed_at);
                }
                if event_type == Some("token_count")
                    && let Some(usage) = codex_total_usage(&value)
                {
                    // `total_token_usage` is already cumulative. Replacing it
                    // avoids double-counting prompt-cache snapshots.
                    parsed.usage = Some(usage);
                }
            }
            _ => {}
        }
    }
    parsed
}

#[cfg(test)]
fn parse_claude(lines: Vec<Vec<u8>>) -> ParsedLog {
    parse_claude_sample(SampledLines {
        lines,
        tail_after_gap: None,
    })
}

fn parse_claude_sample(sample: SampledLines) -> ParsedLog {
    let mut parsed = ParsedLog::default();
    let mut agent_id = None;
    let mut is_sidechain = false;
    let mut message_usages = HashMap::new();
    for (index, line) in sample.lines.into_iter().enumerate() {
        if sample.tail_after_gap == Some(index) {
            parsed.state = LogState::Unknown;
            parsed.state_at = None;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        parsed.id = string_at(&value, &["sessionId"]).or(parsed.id);
        parsed.cwd = string_at(&value, &["cwd"]).or(parsed.cwd);
        agent_id = string_at(&value, &["agentId"]).or(agent_id);
        is_sidechain |= value
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let kind = value.get("type").and_then(Value::as_str);
        if kind == Some("custom-title") {
            parsed.title = title_at(&value).or(parsed.title);
        }
        if kind == Some("user") {
            parsed.state = LogState::ClaudeUser;
            parsed.state_at = record_timestamp(&value);
        } else if kind == Some("assistant") {
            parsed.state_at = record_timestamp(&value);
            let message = value.get("message");
            parsed.model = message
                .and_then(|message| string_at(message, &["model"]))
                .or(parsed.model);
            if let Some((usage_id, item)) = claude_message_usage(&value) {
                // Streaming/resume records may repeat the provider message ID.
                // The newest snapshot replaces the older one; snapshots are
                // never added together.
                message_usages.insert(usage_id, item);
            }
            parsed.state = match message
                .and_then(|message| message.get("stop_reason"))
                .and_then(Value::as_str)
            {
                Some("end_turn" | "stop_sequence") => LogState::ClaudeStopped,
                _ => LogState::ClaudeStreaming,
            };
        }
    }
    if let Some(agent_id) = agent_id {
        if let Some(parent_id) = parsed.id.take() {
            parsed.id = Some(format!("{parent_id}:agent:{agent_id}"));
            parsed.parent_id = Some(parent_id);
        } else {
            parsed.omit_unidentified_child = true;
        }
    } else if is_sidechain {
        parsed.id = None;
        parsed.omit_unidentified_child = true;
    }
    let mut usage = UsageSum::default();
    for item in message_usages.values() {
        usage.add(item);
    }
    // Even a fully sampled transcript can omit usage for compaction/resume
    // records, so Claude usage is never labelled as a provider total.
    parsed.usage = usage.finish(TokenUsageScope::Sampled);
    parsed
}

#[derive(Debug, Default)]
struct UsageSum {
    input_tokens: u128,
    cached_input_tokens: Option<u128>,
    output_tokens: u128,
    reasoning_output_tokens: Option<u128>,
    total_tokens: u128,
    observed: bool,
}

impl UsageSum {
    fn add(&mut self, usage: &TokenUsage) {
        self.input_tokens += u128::from(usage.input_tokens);
        self.cached_input_tokens = match (
            self.observed,
            self.cached_input_tokens,
            usage.cached_input_tokens,
        ) {
            (false, _, value) => value.map(u128::from),
            (true, Some(total), Some(value)) => Some(total + u128::from(value)),
            _ => None,
        };
        self.output_tokens += u128::from(usage.output_tokens);
        self.reasoning_output_tokens = match (
            self.observed,
            self.reasoning_output_tokens,
            usage.reasoning_output_tokens,
        ) {
            (false, _, value) => value.map(u128::from),
            (true, Some(total), Some(value)) => Some(total + u128::from(value)),
            _ => None,
        };
        self.total_tokens += u128::from(usage.total_tokens);
        self.observed = true;
    }

    fn finish(self, scope: TokenUsageScope) -> Option<TokenUsage> {
        if !self.observed {
            return None;
        }
        Some(TokenUsage {
            input_tokens: self.input_tokens.try_into().ok()?,
            cached_input_tokens: self
                .cached_input_tokens
                .map(u64::try_from)
                .transpose()
                .ok()?,
            output_tokens: self.output_tokens.try_into().ok()?,
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .map(u64::try_from)
                .transpose()
                .ok()?,
            total_tokens: self.total_tokens.try_into().ok()?,
            scope,
        })
    }
}

fn claude_message_usage(value: &Value) -> Option<(String, TokenUsage)> {
    let message = value.get("message")?;
    // Provider message IDs are stable across streaming transcript updates.
    // A top-level transcript UUID identifies the record, not the API message,
    // so records without `message.id` cannot be deduplicated reliably.
    let usage_id = string_at(message, &["id"])?;
    let usage = message.get("usage")?;
    let base_input = u64_at(usage, "input_tokens")?;
    let cache_creation = u64_at(usage, "cache_creation_input_tokens").unwrap_or(0);
    let cache_read = u64_at(usage, "cache_read_input_tokens");
    let input_tokens = base_input
        .checked_add(cache_creation)?
        .checked_add(cache_read.unwrap_or(0))?;
    let output_tokens = u64_at(usage, "output_tokens")?;
    let total_tokens = input_tokens.checked_add(output_tokens)?;
    let reasoning_output_tokens = u64_at(usage, "thinking_tokens").or_else(|| {
        usage
            .get("output_tokens_details")
            .and_then(|details| u64_at(details, "thinking_tokens"))
    });
    if reasoning_output_tokens.is_some_and(|reasoning| reasoning > output_tokens) {
        return None;
    }
    Some((
        usage_id,
        TokenUsage {
            input_tokens,
            cached_input_tokens: cache_read,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
            scope: TokenUsageScope::Sampled,
        },
    ))
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn record_timestamp(value: &Value) -> Option<i64> {
    value.get("timestamp").and_then(|timestamp| {
        timestamp
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp())
    })
}

fn activity(state: LogState, live: bool, lifecycle_is_fresh: bool) -> Activity {
    if live && !lifecycle_is_fresh {
        return Activity::Unknown;
    }
    match state {
        LogState::Started | LogState::ClaudeUser | LogState::ClaudeStreaming if live => {
            Activity::Working
        }
        LogState::Completed | LogState::Aborted | LogState::ClaudeStopped => Activity::Idle,
        _ => Activity::Unknown,
    }
}

fn activity_observation(
    provider: Provider,
    state: LogState,
    observed_at: Option<i64>,
) -> Option<ActivityObservation> {
    let source = match provider {
        Provider::Codex => "codex_log",
        Provider::Claude => "claude_log",
        _ => return None,
    };
    let event = match state {
        LogState::Started => "turn_activity",
        LogState::Completed => "task_complete",
        LogState::Aborted => "turn_aborted",
        LogState::ClaudeUser => "user",
        LogState::ClaudeStreaming => "assistant",
        LogState::ClaudeStopped => "end_turn",
        LogState::Unknown => return None,
    };
    Some(ActivityObservation {
        source: source.to_owned(),
        event: event.to_owned(),
        observed_at: observed_at?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(file: &mut File, value: Value) {
        serde_json::to_writer(&mut *file, &value).unwrap();
        file.write_all(b"\n").unwrap();
    }

    #[test]
    fn codex_ignores_partial_json_and_extracts_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"{partial}\n").unwrap();
        line(
            &mut file,
            serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": "session-1",
                    "cwd": "/work",
                    "title": "Synthetic session",
                    "source": {"subagent": {"spawn": {"parent_thread_id": "parent-1"}}}
                }
            }),
        );
        line(
            &mut file,
            serde_json::json!({"type":"turn_context","payload":{"model":"gpt-test"}}),
        );
        line(
            &mut file,
            serde_json::json!({"type":"event_msg","payload":{"type":"task_started"}}),
        );
        file.write_all(b"{\"type\":").unwrap();
        drop(file);

        let parsed = parse_log(&path, &Provider::Codex).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("session-1"));
        assert_eq!(parsed.parent_id.as_deref(), Some("parent-1"));
        assert_eq!(parsed.cwd.as_deref(), Some("/work"));
        assert_eq!(parsed.model.as_deref(), Some("gpt-test"));
        assert_eq!(parsed.title.as_deref(), Some("Synthetic session"));
        assert_eq!(activity(parsed.state, true, true), Activity::Working);
        assert_eq!(activity(parsed.state, false, true), Activity::Unknown);
    }

    #[test]
    fn codex_uses_latest_cumulative_usage_snapshot_without_summing() {
        let parsed = parse_codex(vec![
            br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120}}}}"#.to_vec(),
            br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":175,"cached_input_tokens":70,"output_tokens":35,"reasoning_output_tokens":8,"total_tokens":210}}}}"#.to_vec(),
        ]);
        assert_eq!(
            parsed.usage,
            Some(TokenUsage {
                input_tokens: 175,
                cached_input_tokens: Some(70),
                output_tokens: 35,
                reasoning_output_tokens: Some(8),
                total_tokens: 210,
                scope: TokenUsageScope::Total,
            })
        );

        let across_gap = parse_codex_sample(SampledLines {
            lines: vec![
                br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120}}}}"#.to_vec(),
                br#"{"type":"event_msg","payload":{"type":"task_complete"}}"#.to_vec(),
            ],
            tail_after_gap: Some(1),
        });
        assert_eq!(across_gap.usage, None);
    }

    #[test]
    fn codex_title_index_accepts_only_named_metadata_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.jsonl");
        let mut file = File::create(&path).unwrap();
        line(
            &mut file,
            serde_json::json!({"id":"one","thread_name":" First title "}),
        );
        line(
            &mut file,
            serde_json::json!({"id":"one","custom_title":"Latest title"}),
        );
        line(
            &mut file,
            serde_json::json!({"id":"two","first_prompt":"must not become a title"}),
        );
        drop(file);

        let titles = load_codex_titles(&path).unwrap();
        assert_eq!(titles.get("one").map(String::as_str), Some("Latest title"));
        assert!(!titles.contains_key("two"));
    }

    #[test]
    fn optional_usage_fields_preserve_missing_and_explicit_zero() {
        let missing = parse_codex(vec![
            br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}}"#.to_vec(),
        ])
        .usage
        .unwrap();
        assert_eq!(missing.cached_input_tokens, None);
        assert_eq!(missing.reasoning_output_tokens, None);

        let explicit_zero = parse_codex(vec![
            br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2,"reasoning_output_tokens":0,"total_tokens":12}}}}"#.to_vec(),
        ])
        .usage
        .unwrap();
        assert_eq!(explicit_zero.cached_input_tokens, Some(0));
        assert_eq!(explicit_zero.reasoning_output_tokens, Some(0));

        let sampled = parse_claude(vec![
            br#"{"type":"assistant","sessionId":"claude-1","message":{"id":"message-1","usage":{"input_tokens":10,"cache_read_input_tokens":0,"output_tokens":2,"thinking_tokens":0}}}"#.to_vec(),
            br#"{"type":"assistant","sessionId":"claude-1","message":{"id":"message-2","usage":{"input_tokens":20,"output_tokens":3}}}"#.to_vec(),
        ])
        .usage
        .unwrap();
        assert_eq!(sampled.cached_input_tokens, None);
        assert_eq!(sampled.reasoning_output_tokens, None);
    }

    #[test]
    fn rejects_missing_tty_and_dead_process_statuses() {
        for tty in ["", "?", "??", "-", "/dev/??", "../tty", "tty x", "/tmp/tty"] {
            assert!(normalized_tty(tty).is_none(), "{tty}");
        }
        assert_eq!(normalized_tty(" ttys003\n").as_deref(), Some("ttys003"));
        assert_eq!(normalized_tty("/dev/pts/2").as_deref(), Some("pts/2"));
        assert!(!process_is_live(ProcessStatus::Zombie));
        assert!(!process_is_live(ProcessStatus::Dead));
        assert!(process_is_live(ProcessStatus::Sleep));
        assert!(process_is_live(ProcessStatus::Stop));
        assert!(process_is_live(ProcessStatus::UninterruptibleDiskSleep));
    }

    #[test]
    fn codex_lifecycle_requires_fresh_events_and_completion_is_idle() {
        let parsed = parse_codex(vec![
            br#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
            br#"{"timestamp":"2026-09-15T00:00:00Z","type":"turn_context","payload":{"model":"test-model"}}"#.to_vec(),
        ]);
        assert_eq!(parsed.state_at, Some(1767225600));
        assert_eq!(activity(parsed.state, true, false), Activity::Unknown);

        let untimed = parse_codex(vec![
            br#"{"type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
        ]);
        assert!(untimed.state_at.is_none());
        let started = parse_codex(vec![
            br#"{"type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
        ]);
        let completed = parse_codex(vec![
            br#"{"type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
            br#"{"type":"event_msg","payload":{"type":"task_complete"}}"#.to_vec(),
        ]);
        assert_eq!(activity(started.state, true, true), Activity::Working);
        assert_eq!(activity(completed.state, true, true), Activity::Idle);
        assert_eq!(activity(LogState::Started, true, false), Activity::Unknown);
        assert_eq!(
            activity(LogState::Completed, true, false),
            Activity::Unknown
        );
    }

    #[test]
    fn codex_progress_does_not_cross_a_gap_but_refreshes_a_sampled_start() {
        let across_gap = parse_codex_sample(SampledLines {
            lines: vec![
                br#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
                br#"{"timestamp":"2026-01-01T00:10:00Z","type":"event_msg","payload":{"type":"item_completed"}}"#.to_vec(),
            ],
            tail_after_gap: Some(1),
        });
        assert_eq!(across_gap.state, LogState::Unknown);
        assert_eq!(across_gap.state_at, None);

        let refreshed = parse_codex(vec![
            br#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
            br#"{"timestamp":"2026-01-01T00:10:00Z","type":"event_msg","payload":{"type":"token_count"}}"#.to_vec(),
        ]);
        assert_eq!(refreshed.state, LogState::Started);
        assert_eq!(refreshed.state_at, Some(1767226200));
    }

    #[test]
    fn codex_progress_never_reopens_a_terminal_state() {
        for terminal_event in ["task_complete", "turn_aborted"] {
            let terminal = format!(
                r#"{{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{{"type":"{terminal_event}"}}}}"#
            );
            let parsed = parse_codex(vec![
                br#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
                terminal.into_bytes(),
                br#"{"timestamp":"2026-01-01T00:02:00Z","type":"event_msg","payload":{"type":"token_count"}}"#.to_vec(),
                br#"{"timestamp":"2026-01-01T00:03:00Z","type":"event_msg","payload":{"type":"item_completed"}}"#.to_vec(),
            ]);
            assert_eq!(
                parsed.state,
                if terminal_event == "task_complete" {
                    LogState::Completed
                } else {
                    LogState::Aborted
                }
            );
            assert_eq!(parsed.state_at, Some(1767225660));
        }

        let restarted = parse_codex(vec![
            br#"{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{"type":"task_complete"}}"#.to_vec(),
            br#"{"timestamp":"2026-01-01T00:02:00Z","type":"event_msg","payload":{"type":"token_count"}}"#.to_vec(),
            br#"{"timestamp":"2026-01-01T00:03:00Z","type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
        ]);
        assert_eq!(restarted.state, LogState::Started);
        assert_eq!(restarted.state_at, Some(1767225780));
    }

    #[test]
    fn codex_progress_cannot_establish_state_and_requires_a_timestamp_to_refresh() {
        let no_lifecycle = parse_codex(vec![
            br#"{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{"type":"token_count"}}"#.to_vec(),
            br#"{"timestamp":"2026-01-01T00:02:00Z","type":"event_msg","payload":{"type":"item_completed"}}"#.to_vec(),
        ]);
        assert_eq!(no_lifecycle.state, LogState::Unknown);
        assert_eq!(no_lifecycle.state_at, None);

        let missing = parse_codex(vec![
            br#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
            br#"{"type":"event_msg","payload":{"type":"token_count"}}"#.to_vec(),
            br#"{"type":"event_msg","payload":{"type":"item_completed"}}"#.to_vec(),
        ]);
        assert_eq!(missing.state, LogState::Started);
        assert_eq!(missing.state_at, Some(1767225600));
    }

    #[test]
    fn activity_observations_use_stable_provider_event_names() {
        for (provider, state, source, event) in [
            (
                Provider::Codex,
                LogState::Started,
                "codex_log",
                "turn_activity",
            ),
            (
                Provider::Codex,
                LogState::Completed,
                "codex_log",
                "task_complete",
            ),
            (
                Provider::Codex,
                LogState::Aborted,
                "codex_log",
                "turn_aborted",
            ),
            (Provider::Claude, LogState::ClaudeUser, "claude_log", "user"),
            (
                Provider::Claude,
                LogState::ClaudeStreaming,
                "claude_log",
                "assistant",
            ),
            (
                Provider::Claude,
                LogState::ClaudeStopped,
                "claude_log",
                "end_turn",
            ),
        ] {
            let observation = activity_observation(provider, state, Some(42)).unwrap();
            assert_eq!(observation.source, source);
            assert_eq!(observation.event, event);
            assert_eq!(observation.observed_at, 42);
        }
        assert!(activity_observation(Provider::Codex, LogState::Unknown, Some(42)).is_none());
        assert!(activity_observation(Provider::Codex, LogState::Started, None).is_none());
    }

    #[test]
    fn parent_id_comes_only_from_spawn_metadata() {
        let unrelated = parse_codex(vec![
            br#"{"type":"session_meta","payload":{"id":"s","source":{"parent_thread_id":"invent-me"}}}"#
                .to_vec(),
        ]);
        assert_eq!(unrelated.parent_id, None);

        let direct_preferred = parse_codex(vec![
            br#"{"type":"session_meta","payload":{"id":"s","parent_thread_id":"direct-parent","source":{"subagent":{"thread_spawn":{"parent_thread_id":"nested-parent"}}}}}"#.to_vec(),
        ]);
        assert_eq!(direct_preferred.parent_id.as_deref(), Some("direct-parent"));

        let current_nested = parse_codex(vec![
            br#"{"type":"session_meta","payload":{"id":"s","source":{"subagent":{"thread_spawn":{"parent_thread_id":"real-parent"}}}}}"#.to_vec(),
        ]);
        assert_eq!(current_nested.parent_id.as_deref(), Some("real-parent"));

        let legacy_nested = parse_codex(vec![
            br#"{"type":"session_meta","payload":{"id":"s","source":{"subagent":{"spawn":{"parent_thread_id":"legacy-parent"}}}}}"#.to_vec(),
        ]);
        assert_eq!(legacy_nested.parent_id.as_deref(), Some("legacy-parent"));
    }

    #[test]
    fn bounded_reader_handles_a_long_gap_and_exact_seams() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.jsonl");
        let mut file = File::create(&path).unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"session_meta","payload":{"id":"bounded"}}),
        );
        file.write_all(&vec![b'x'; (HEAD_BYTES + TAIL_BYTES + 1024) as usize])
            .unwrap();
        file.write_all(b"\n").unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        );
        drop(file);

        let parsed = parse_log(&path, &Provider::Codex).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("bounded"));
        assert_eq!(parsed.state, LogState::Completed);

        let seam_path = dir.path().join("seam.jsonl");
        fs::write(&seam_path, b"first\nsecond\n").unwrap();
        let mut file = File::open(&seam_path).unwrap();
        let lines = read_complete_range(&mut file, 6, 7, 13).unwrap();
        assert_eq!(lines, vec![b"second".to_vec()]);

        let mut file = File::open(seam_path).unwrap();
        let fragments = read_complete_range(&mut file, 8, 5, 13).unwrap();
        assert!(fragments.is_empty());
    }

    #[test]
    fn codex_model_lookback_respects_boundaries_and_partial_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-lookback.jsonl");
        let mut file = File::create(&path).unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"session_meta","payload":{"id":"bounded"}}),
        );
        line(
            &mut file,
            serde_json::json!({"type":"turn_context","payload":{"model":"gpt-5.6-sol"}}),
        );
        file.write_all(&vec![b'x'; HEAD_BYTES as usize]).unwrap();
        file.write_all(b"\n").unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"turn_context","payload":{"model":"gpt-6-astra"}}),
        );
        file.write_all(&vec![b'x'; (TAIL_BYTES + 1024) as usize])
            .unwrap();
        file.write_all(b"\n").unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        );
        drop(file);

        let sample = bounded_lines(&path).unwrap();
        let tail_start = sample.tail_after_gap.unwrap();
        assert!(
            !sample.lines[tail_start..]
                .iter()
                .any(|line| codex_model(line).is_some())
        );
        assert_eq!(
            parse_codex_sample(sample).model.as_deref(),
            Some("gpt-5.6-sol")
        );
        let parsed = parse_log(&path, &Provider::Codex).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("gpt-6-astra"));

        let stale_path = dir.path().join("stale-model.jsonl");
        let mut file = File::create(&stale_path).unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"session_meta","payload":{"id":"bounded"}}),
        );
        line(
            &mut file,
            serde_json::json!({"type":"turn_context","payload":{"model":"stale"}}),
        );
        file.write_all(&vec![
            b'x';
            (MODEL_LOOKBACK_BYTES + TAIL_BYTES + 1024) as usize
        ])
        .unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        let parsed = parse_log(&stale_path, &Provider::Codex).unwrap();
        assert_eq!(parsed.model, None);

        let partial_path = dir.path().join("partial-model.jsonl");
        let mut file = File::create(&partial_path).unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"session_meta","payload":{"id":"bounded"}}),
        );
        line(
            &mut file,
            serde_json::json!({"type":"turn_context","payload":{"model":"gpt-5.6-sol"}}),
        );
        file.write_all(&vec![b'x'; HEAD_BYTES as usize]).unwrap();
        file.write_all(b"\n").unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"turn_context","payload":{"model":"gpt-6-astra"}}),
        );
        file.write_all(&vec![b'x'; (TAIL_BYTES + 1024) as usize])
            .unwrap();
        file.write_all(b"\n").unwrap();
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({"type":"turn_context","payload":{"model":"unfinished"}}),
        )
        .unwrap();
        drop(file);

        let parsed = parse_log(&partial_path, &Provider::Codex).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("gpt-6-astra"));
    }

    #[test]
    fn claude_extracts_only_status_metadata() {
        let parsed = parse_claude(vec![
            br#"{"type":"user","sessionId":"claude-1","cwd":"/work","message":{"content":"not returned"}}"#.to_vec(),
            br#"{"type":"assistant","sessionId":"claude-1","message":{"model":"claude-test","stop_reason":"end_turn","content":"not returned"}}"#.to_vec(),
        ]);
        assert_eq!(parsed.id.as_deref(), Some("claude-1"));
        assert_eq!(parsed.cwd.as_deref(), Some("/work"));
        assert_eq!(parsed.model.as_deref(), Some("claude-test"));
        assert_eq!(activity(parsed.state, true, true), Activity::Idle);
        assert_eq!(parsed.parent_id, None);
    }

    #[test]
    fn claude_custom_title_and_message_usage_are_bounded_and_deduplicated() {
        let lines = vec![
            br#"{"type":"custom-title","customTitle":"Release audit"}"#.to_vec(),
            br#"{"type":"assistant","sessionId":"claude-1","uuid":"record-1","message":{"id":"message-1","model":"claude-test","stop_reason":"end_turn","usage":{"input_tokens":10,"cache_creation_input_tokens":3,"cache_read_input_tokens":5,"output_tokens":7,"thinking_tokens":2}}}"#.to_vec(),
            br#"{"type":"assistant","sessionId":"claude-1","uuid":"duplicate-record","message":{"id":"message-1","model":"claude-test","stop_reason":"end_turn","usage":{"input_tokens":12,"cache_creation_input_tokens":4,"cache_read_input_tokens":6,"output_tokens":8,"thinking_tokens":3}}}"#.to_vec(),
            br#"{"type":"assistant","sessionId":"claude-1","uuid":"record-2","message":{"id":"message-2","model":"claude-test","stop_reason":"end_turn","usage":{"input_tokens":20,"cache_read_input_tokens":4,"output_tokens":9,"output_tokens_details":{"thinking_tokens":3}}}}"#.to_vec(),
        ];
        let parsed = parse_claude(lines.clone());
        assert_eq!(parsed.title.as_deref(), Some("Release audit"));
        assert_eq!(
            parsed.usage,
            Some(TokenUsage {
                input_tokens: 46,
                cached_input_tokens: Some(10),
                output_tokens: 17,
                reasoning_output_tokens: Some(6),
                total_tokens: 63,
                scope: TokenUsageScope::Sampled,
            })
        );

        let sampled = parse_claude_sample(SampledLines {
            lines,
            tail_after_gap: Some(2),
        });
        assert_eq!(
            sampled.usage.as_ref().map(|usage| usage.scope),
            Some(TokenUsageScope::Sampled)
        );
    }

    #[test]
    fn conversation_excerpt_keeps_only_recent_plaintext_messages() {
        let lines = vec![
            br#"{"type":"response_item","payload":{"type":"message","role":"system","content":[{"type":"input_text","text":"hidden"}]}}"#.to_vec(),
            br#"{"type":"response_item","payload":{"type":"function_call","arguments":"hidden tool args"}}"#.to_vec(),
            br#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}"#.to_vec(),
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"second"},{"type":"reasoning","text":"hidden reasoning"}]}}"#.to_vec(),
            br#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"third"}]}}"#.to_vec(),
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fourth\u0007"}]}}"#.to_vec(),
        ];
        assert_eq!(
            recent_plaintext_messages(Provider::Codex, lines),
            vec![
                ("Assistant", "second".to_owned()),
                ("User", "third".to_owned()),
                ("Assistant", "fourth ".to_owned()),
            ]
        );

        let claude = recent_plaintext_messages(
            Provider::Claude,
            vec![
                br#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"hidden"}]}}"#.to_vec(),
                br#"{"type":"user","isMeta":true,"message":{"role":"user","content":"hidden metadata"}}"#.to_vec(),
                br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"visible"}]}}"#.to_vec(),
            ],
        );
        assert_eq!(claude, vec![("Assistant", "visible".to_owned())]);
    }

    #[test]
    fn shared_open_file_is_not_assigned_arbitrarily() {
        let shared = PathBuf::from("/tmp/shared.jsonl");
        let process = |pid| LiveProcess {
            provider: Provider::Codex,
            pid,
            started_at: 1,
            tty: None,
            cwd: None,
            headless: false,
            open_files: HashSet::from([shared.clone()]),
            open_files_complete: true,
        };
        let map = unique_process_file_map(&[process(1), process(2)]);
        assert!(!map.contains_key(&shared));
    }

    #[test]
    fn incomplete_provider_descriptor_census_rejects_conversation_ownership() {
        let path = PathBuf::from("/tmp/session.jsonl");
        let owner = LiveProcess {
            provider: Provider::Codex,
            pid: 7,
            started_at: 11,
            tty: None,
            cwd: None,
            headless: false,
            open_files: HashSet::from([path.clone()]),
            open_files_complete: true,
        };
        let mut uninspected = owner.clone();
        uninspected.pid = 8;
        uninspected.open_files.clear();
        uninspected.open_files_complete = false;
        let session = Session {
            id: "session".into(),
            provider: Provider::Codex,
            parent_id: None,
            host: "local".into(),
            pid: Some(7),
            process_started_at: Some(11),
            tty: None,
            cwd: None,
            model: None,
            activity: Activity::Unknown,
            confidence: Confidence::Inferred,
            evidence: String::new(),
            updated_at: None,
            target: None,
            insights: SessionInsights::default(),
        };

        let error = verify_conversation_owner(&[owner, uninspected], &session, &path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("could not be fully revalidated"));
    }

    #[cfg(unix)]
    #[test]
    fn open_conversation_file_rejects_a_replaced_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        File::create(&path).unwrap();
        let open = File::open(&path).unwrap();
        assert!(open_file_matches_path(&open, &path).unwrap());

        fs::rename(&path, dir.path().join("previous.jsonl")).unwrap();
        File::create(&path).unwrap();

        assert!(!open_file_matches_path(&open, &path).unwrap());
    }

    #[test]
    fn descriptor_owner_must_match_log_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrong-provider.jsonl");
        File::create(&path).unwrap();
        let canonical = fs::canonicalize(&path).unwrap();
        let owners = HashMap::from([(canonical, (0, Provider::Claude))]);
        let mut scan = LogScan::default();
        gather_logs(dir.path(), Provider::Codex, UNIX_EPOCH, &owners, &mut scan);
        assert_eq!(scan.logs.len(), 1);
        assert_eq!(scan.logs[0].matched_process, None);
    }

    #[test]
    fn live_descriptor_owned_log_survives_recent_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let live_path = dir.path().join("live.jsonl");
        let historical_path = dir.path().join("historical.jsonl");
        let wrong_provider_path = dir.path().join("wrong-provider.jsonl");
        let old_time = UNIX_EPOCH + Duration::from_secs(10);
        for path in [&live_path, &historical_path, &wrong_provider_path] {
            let file = File::create(path).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(old_time))
                .unwrap();
        }
        let owners = HashMap::from([
            (fs::canonicalize(&live_path).unwrap(), (0, Provider::Codex)),
            (
                fs::canonicalize(&wrong_provider_path).unwrap(),
                (1, Provider::Claude),
            ),
        ]);
        let mut scan = LogScan::default();

        gather_logs(
            dir.path(),
            Provider::Codex,
            UNIX_EPOCH + Duration::from_secs(20),
            &owners,
            &mut scan,
        );

        assert_eq!(scan.logs.len(), 1);
        assert_eq!(scan.logs[0].path, live_path);
        assert_eq!(scan.logs[0].matched_process, Some(0));
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn lsof_parser_accepts_only_writable_descriptors() {
        let output = b"p42\nf1\nar\nn/read-only.jsonl\nf2\naw\nn/write-only.jsonl\nf3\nau\nn/read-write.jsonl\n";
        assert_eq!(
            lsof_writable_names(output),
            vec![
                b"/write-only.jsonl".as_slice(),
                b"/read-write.jsonl".as_slice()
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_fdinfo_parser_accepts_only_writable_descriptors() {
        assert!(!linux_fdinfo_is_writable("flags:\t0100000\n"));
        assert!(linux_fdinfo_is_writable("flags:\t0100001\n"));
        assert!(linux_fdinfo_is_writable("flags:\t0100002\n"));
    }

    #[test]
    fn lifecycle_does_not_cross_an_unread_middle() {
        let parsed = parse_codex_sample(SampledLines {
            lines: vec![
                br#"{"type":"event_msg","payload":{"type":"task_started"}}"#.to_vec(),
                br#"{"type":"turn_context","payload":{"model":"gpt-test"}}"#.to_vec(),
            ],
            tail_after_gap: Some(1),
        });
        assert_eq!(parsed.state, LogState::Unknown);
        assert_eq!(parsed.model.as_deref(), Some("gpt-test"));
    }

    #[test]
    fn claude_tool_use_is_not_idle_and_children_are_distinct() {
        let tool_use = parse_claude(vec![
            br#"{"type":"assistant","sessionId":"parent","message":{"stop_reason":"tool_use"}}"#
                .to_vec(),
        ]);
        assert_eq!(tool_use.state, LogState::ClaudeStreaming);

        let child = parse_claude(vec![
            br#"{"type":"assistant","sessionId":"parent","agentId":"child-7","isSidechain":true,"message":{"stop_reason":"end_turn"}}"#.to_vec(),
        ]);
        assert_eq!(child.id.as_deref(), Some("parent:agent:child-7"));
        assert_eq!(child.parent_id.as_deref(), Some("parent"));
        assert!(!child.omit_unidentified_child);

        let unidentified = parse_claude(vec![
            br#"{"type":"assistant","sessionId":"parent","isSidechain":true,"message":{"stop_reason":"end_turn"}}"#.to_vec(),
        ]);
        assert!(unidentified.id.is_none());
        assert!(unidentified.omit_unidentified_child);
    }

    #[test]
    fn collection_labels_unmatched_logs_as_historical() {
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("codex");
        let sessions = codex_home.join("sessions/2026/09/15");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("historical.jsonl");
        let mut file = File::create(path).unwrap();
        line(
            &mut file,
            serde_json::json!({"type":"session_meta","payload":{"id":"historical-session"}}),
        );
        line(
            &mut file,
            serde_json::json!({"timestamp":"2026-09-15T00:00:00Z","type":"event_msg","payload":{"type":"task_started"}}),
        );
        drop(file);

        let snapshot = collect(&CollectOptions {
            codex_home,
            claude_home: dir.path().join("claude"),
            recent_minutes: 60,
            max_logs: 10,
        })
        .unwrap();
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == "historical-session")
            .unwrap();
        assert_eq!(session.pid, None);
        assert_eq!(session.activity, Activity::Unknown);
        assert_eq!(session.confidence, Confidence::Inferred);
        assert!(session.evidence.contains("historical"));
        assert_eq!(
            session.insights.activity_observation,
            Some(ActivityObservation {
                source: "codex_log".to_owned(),
                event: "turn_activity".to_owned(),
                observed_at: 1789430400,
            })
        );
    }
}
