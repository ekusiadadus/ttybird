//! Read-only runtime status from an already-running local Codex app-server.
//! No daemon launch, subscription, thread resume, model call, or approval reply.
use crate::model::{Activity, ActivityObservation, Confidence, Provider, Session, Snapshot};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tungstenite::{
    Error as WebSocketError, Message, client::client_with_config, protocol::WebSocketConfig,
};

const BUDGET: Duration = Duration::from_millis(1500);
const MAX_THREADS: usize = 64;
const MAX_MESSAGE: usize = 1024 * 1024;
const WATCH_IO_SLICE: Duration = Duration::from_millis(250);
const WATCH_RETRY: Duration = Duration::from_millis(250);
const WATCH_MAX_RETRY: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct Reply {
    id: Option<u64>,
    result: Option<ReadResult>,
}
#[derive(Deserialize)]
struct ReadResult {
    thread: Option<Thread>,
}
#[derive(Deserialize)]
struct Thread {
    id: String,
    status: Status,
}
#[derive(Clone, Deserialize)]
#[serde(tag = "type")]
enum Status {
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "active")]
    Active {
        #[serde(rename = "activeFlags")]
        flags: Vec<String>,
    },
    #[serde(rename = "systemError")]
    SystemError,
    #[serde(other)]
    Other,
}

#[derive(Clone, PartialEq, Eq)]
struct DirectStatus {
    activity: Activity,
    event: &'static str,
    observed_at: i64,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WatchTarget {
    id: String,
    pid: u32,
    process_started_at: u64,
}

#[derive(Default)]
struct WatchState {
    targets: Vec<WatchTarget>,
    target_revision: u64,
    statuses: HashMap<String, DirectStatus>,
}

struct WatchShared {
    state: Mutex<WatchState>,
    wake: Condvar,
    changed: AtomicBool,
    stop: AtomicBool,
}

/// Passive live-status observer for the TUI.
///
/// The worker talks only to an already-running, same-user Codex control socket.
/// It never resumes or subscribes to a thread and retains no thread content.
pub struct Watcher {
    shared: Arc<WatchShared>,
    worker: Option<JoinHandle<()>>,
}

impl Watcher {
    pub fn spawn() -> Self {
        Self::spawn_at(socket_path())
    }

    fn spawn_at(path: Option<PathBuf>) -> Self {
        let shared = Arc::new(WatchShared {
            state: Mutex::new(WatchState::default()),
            wake: Condvar::new(),
            changed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("codex-status-observer".into())
            .spawn(move || watch_worker(worker_shared, path))
            .ok();
        Self { shared, worker }
    }

    /// Replace the bounded set of live local Codex thread identities to observe.
    pub fn update_targets(&self, snapshot: &Snapshot) {
        let mut targets = watch_targets(snapshot);
        targets.sort_unstable();
        targets.dedup_by(|left, right| left.id == right.id);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.targets == targets {
            return;
        }
        state.targets = targets;
        state.target_revision = state.target_revision.wrapping_add(1);
        if !state.statuses.is_empty() {
            state.statuses.clear();
            self.shared.changed.store(true, Ordering::Release);
        }
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Apply the current connected-generation cache to a freshly collected snapshot.
    pub fn apply(&self, snapshot: &mut Snapshot) {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        for session in &mut snapshot.sessions {
            if !is_live_codex(session) {
                continue;
            }
            if let Some(status) = state.statuses.get(&session.id) {
                let identity_matches = state.targets.iter().any(|target| {
                    target.id == session.id
                        && Some(target.pid) == session.pid
                        && Some(target.process_started_at) == session.process_started_at
                });
                if identity_matches {
                    apply_direct(session, status);
                }
            }
        }
    }

    /// Drain the coalesced notification that the cache changed.
    pub fn take_changed(&self) -> bool {
        self.shared.changed.swap(false, Ordering::AcqRel)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum WatchExit {
    Stop,
    TargetsChanged,
    Disconnected,
    ConnectedLost,
}

#[derive(Deserialize)]
struct StatusNotification {
    method: String,
    params: StatusNotificationParams,
}

#[derive(Deserialize)]
struct StatusNotificationParams {
    #[serde(rename = "threadId")]
    thread_id: String,
    status: Status,
}

fn watch_worker(shared: Arc<WatchShared>, path: Option<PathBuf>) {
    let Some(path) = path else {
        wait_for_stop(&shared, WATCH_RETRY);
        return;
    };
    let mut retry = WATCH_RETRY;
    while !shared.stop.load(Ordering::Acquire) {
        let (targets, revision) = {
            let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            (state.targets.clone(), state.target_revision)
        };
        if targets.is_empty() {
            invalidate_watch_cache(&shared);
            retry = WATCH_RETRY;
            wait_for_change(&shared, revision, WATCH_MAX_RETRY);
            continue;
        }
        let exit = watch_connection(&shared, &path, &targets, revision);
        invalidate_watch_cache(&shared);
        match exit {
            WatchExit::Stop => break,
            WatchExit::TargetsChanged => retry = WATCH_RETRY,
            WatchExit::ConnectedLost => {
                retry = WATCH_RETRY;
                wait_for_change(&shared, revision, retry);
            }
            WatchExit::Disconnected => {
                wait_for_change(&shared, revision, retry);
                retry = retry.saturating_mul(2).min(WATCH_MAX_RETRY);
            }
        }
    }
}

fn watch_connection(
    shared: &WatchShared,
    path: &Path,
    targets: &[WatchTarget],
    revision: u64,
) -> WatchExit {
    let Ok(stream) = checked_stream(path) else {
        return WatchExit::Disconnected;
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let Ok((mut ws, _)) = client_with_config("ws://localhost/", stream, Some(config)) else {
        return WatchExit::Disconnected;
    };
    let bootstrap_deadline = Instant::now() + BUDGET;
    if send_watch(
        &mut ws,
        Message::text(
            json!({"id":0,"method":"initialize","params":{
                "clientInfo":{"name":"ttybird_observer","version":env!("CARGO_PKG_VERSION")},
                "capabilities":{"experimentalApi":false,"optOutNotificationMethods":["thread/started","thread/name/updated"]}
            }})
            .to_string(),
        ),
        bootstrap_deadline,
    )
    .is_err()
    {
        return WatchExit::Disconnected;
    }
    let mut initialized = false;
    while Instant::now() < bootstrap_deadline && !shared.stop.load(Ordering::Acquire) {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let Ok(reply) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if reply.get("id").and_then(|id| id.as_u64()) == Some(0) {
                    if reply.get("result").is_none() {
                        return WatchExit::Disconnected;
                    }
                    initialized = true;
                    break;
                }
            }
            Ok(_) => {}
            Err(error) if is_idle_timeout(&error) => continue,
            Err(_) => return WatchExit::Disconnected,
        }
    }
    if shared.stop.load(Ordering::Acquire) {
        return WatchExit::Stop;
    }
    if !initialized
        || send_watch(
            &mut ws,
            Message::text(json!({"method":"initialized"}).to_string()),
            bootstrap_deadline,
        )
        .is_err()
    {
        return WatchExit::Disconnected;
    }

    let mut expected = HashMap::new();
    for (index, target) in targets.iter().enumerate() {
        if shared.stop.load(Ordering::Acquire) {
            return WatchExit::Stop;
        }
        if Instant::now() >= bootstrap_deadline {
            return WatchExit::Disconnected;
        }
        let request_id = index as u64 + 1;
        if send_watch(
            &mut ws,
            Message::text(
                json!({"id":request_id,"method":"thread/read","params":{
                    "threadId":target.id,"includeTurns":false
                }})
                .to_string(),
            ),
            bootstrap_deadline,
        )
        .is_err()
        {
            return WatchExit::Disconnected;
        }
        expected.insert(request_id, target.id.as_str());
    }

    let tracked: HashSet<_> = targets.iter().map(|target| target.id.as_str()).collect();
    let mut initial = HashMap::new();
    let mut snapshot_notifications = HashMap::new();
    let mut seen = HashSet::new();
    let mut total_bytes = 0usize;
    while seen.len() != targets.len() {
        if shared.stop.load(Ordering::Acquire) {
            return WatchExit::Stop;
        }
        if current_target_revision(shared) != revision {
            return WatchExit::TargetsChanged;
        }
        if Instant::now() >= bootstrap_deadline {
            return WatchExit::Disconnected;
        }
        let text = match ws.read() {
            Ok(Message::Text(text)) => text,
            Ok(_) => continue,
            Err(error) if is_idle_timeout(&error) => continue,
            Err(_) => return WatchExit::Disconnected,
        };
        total_bytes = total_bytes.saturating_add(text.len());
        if total_bytes > 8 * MAX_MESSAGE {
            return WatchExit::Disconnected;
        }
        if let Ok(notification) = serde_json::from_str::<StatusNotification>(&text)
            && notification.method == "thread/status/changed"
            && tracked.contains(notification.params.thread_id.as_str())
        {
            snapshot_notifications.insert(
                notification.params.thread_id,
                direct_status(notification.params.status, chrono::Utc::now().timestamp()),
            );
            continue;
        }
        let Ok(reply) = serde_json::from_str::<Reply>(&text) else {
            continue;
        };
        let Some(request_id) = reply.id else {
            continue;
        };
        let Some(expected_id) = expected.get(&request_id) else {
            continue;
        };
        if !seen.insert(request_id) {
            continue;
        }
        let Some(thread) = reply.result.and_then(|result| result.thread) else {
            initial.remove(*expected_id);
            continue;
        };
        if thread.id != *expected_id {
            initial.remove(*expected_id);
            continue;
        }
        update_status_map(
            &mut initial,
            thread.id,
            thread.status,
            chrono::Utc::now().timestamp(),
        );
    }
    // A status notification may describe a transition that races a thread/read
    // whose response was sampled earlier but delivered later. Preserve the last
    // notification observed during bootstrap over every read response.
    for (id, status) in snapshot_notifications {
        match status {
            Some(status) => {
                initial.insert(id, status);
            }
            None => {
                initial.remove(&id);
            }
        }
    }
    publish_watch_cache(shared, revision, initial);

    loop {
        if shared.stop.load(Ordering::Acquire) {
            return WatchExit::Stop;
        }
        if current_target_revision(shared) != revision {
            return WatchExit::TargetsChanged;
        }
        let text = match ws.read() {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => return WatchExit::ConnectedLost,
            Ok(_) => continue,
            // An idle control socket is healthy. Retrying the same WebSocket also
            // preserves tungstenite's partial-frame buffer across socket timeouts.
            Err(error) if is_idle_timeout(&error) => continue,
            Err(_) => return WatchExit::ConnectedLost,
        };
        let Ok(notification) = serde_json::from_str::<StatusNotification>(&text) else {
            continue;
        };
        if notification.method != "thread/status/changed"
            || !tracked.contains(notification.params.thread_id.as_str())
        {
            continue;
        }
        let status = direct_status(notification.params.status, chrono::Utc::now().timestamp());
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.target_revision != revision {
            return WatchExit::TargetsChanged;
        }
        let changed = match status {
            Some(status) => {
                state
                    .statuses
                    .insert(notification.params.thread_id, status.clone())
                    .as_ref()
                    != Some(&status)
            }
            None => state
                .statuses
                .remove(&notification.params.thread_id)
                .is_some(),
        };
        drop(state);
        if changed {
            shared.changed.store(true, Ordering::Release);
        }
    }
}

fn checked_stream(path: &Path) -> Result<UnixStream> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("control socket is not owned by the current user");
    }
    let stream = connect(path, Instant::now() + BUDGET)?;
    stream.set_read_timeout(Some(WATCH_IO_SLICE))?;
    stream.set_write_timeout(Some(BUDGET))?;
    Ok(stream)
}

fn send_watch(
    ws: &mut tungstenite::WebSocket<UnixStream>,
    message: Message,
    deadline: Instant,
) -> Result<(), WebSocketError> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(WebSocketError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "observer bootstrap deadline",
        )));
    };
    ws.get_mut()
        .set_write_timeout(Some(remaining.min(WATCH_IO_SLICE)))
        .map_err(WebSocketError::Io)?;
    ws.send(message)
}

fn is_idle_timeout(error: &WebSocketError) -> bool {
    matches!(
        error,
        WebSocketError::Io(error)
            if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
    )
}

fn current_target_revision(shared: &WatchShared) -> u64 {
    shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .target_revision
}

fn publish_watch_cache(
    shared: &WatchShared,
    revision: u64,
    statuses: HashMap<String, DirectStatus>,
) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.target_revision != revision {
        return;
    }
    if state.statuses != statuses {
        state.statuses = statuses;
        shared.changed.store(true, Ordering::Release);
    }
}

fn invalidate_watch_cache(shared: &WatchShared) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.statuses.is_empty() {
        state.statuses.clear();
        shared.changed.store(true, Ordering::Release);
    }
}

fn wait_for_change(shared: &WatchShared, revision: u64, duration: Duration) {
    let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if shared.stop.load(Ordering::Acquire) || state.target_revision != revision {
        return;
    }
    let _ = shared
        .wake
        .wait_timeout(state, duration)
        .unwrap_or_else(|e| e.into_inner());
}

fn wait_for_stop(shared: &WatchShared, duration: Duration) {
    while !shared.stop.load(Ordering::Acquire) {
        let revision = current_target_revision(shared);
        wait_for_change(shared, revision, duration);
    }
}

fn update_status_map(
    statuses: &mut HashMap<String, DirectStatus>,
    id: String,
    status: Status,
    observed_at: i64,
) {
    match direct_status(status, observed_at) {
        Some(status) => {
            statuses.insert(id, status);
        }
        None => {
            statuses.remove(&id);
        }
    }
}

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".codex")))?;
    Some(home.join("app-server-control/app-server-control.sock"))
}

pub fn enrich(snapshot: &mut Snapshot) {
    let Some(path) = socket_path() else { return };
    // A missing daemon is normal for embedded/older Codex. Never start one.
    if !path.exists() {
        return;
    }
    if let Err(error) = enrich_at(snapshot, &path) {
        snapshot.warnings.push(format!(
            "Codex live status unavailable ({error}); showing log evidence"
        ));
    }
}

fn enrich_at(snapshot: &mut Snapshot, path: &Path) -> Result<()> {
    let ids = thread_ids(snapshot);
    if ids.is_empty() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("control socket is not owned by the current user");
    }
    let deadline = Instant::now() + BUDGET;
    let stream = DeadlineStream {
        stream: connect(path, deadline)?,
        deadline,
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (mut ws, _) = client_with_config("ws://localhost/", stream, Some(config))
        .map_err(|_| anyhow::anyhow!("control socket handshake failed"))?;
    ws.send(Message::text(json!({"id":0,"method":"initialize","params":{
        "clientInfo":{"name":"ttybird_observer","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":false,"optOutNotificationMethods":["thread/started","thread/status/changed","thread/name/updated"]}
    }}).to_string()))?;
    // Initialization contains no thread content. Only recognize the response envelope.
    let mut initialized = false;
    for _ in 0..16 {
        let message = ws.read().context("initialize timed out")?;
        if let Message::Text(text) = message {
            let reply: serde_json::Value = serde_json::from_str(&text)?;
            if reply.get("id").and_then(|id| id.as_u64()) == Some(0) {
                if reply.get("result").is_none() {
                    bail!("server rejected observer initialization");
                }
                initialized = true;
                break;
            }
        }
    }
    if !initialized {
        bail!("missing initialize response");
    }
    ws.send(Message::text(json!({"method":"initialized"}).to_string()))?;
    for (index, id) in ids.iter().enumerate() {
        ws.send(Message::text(json!({"id":index+1,"method":"thread/read","params":{"threadId":id,"includeTurns":false}}).to_string()))?;
    }
    let mut remaining = ids.len();
    let mut seen = std::collections::HashSet::new();
    let mut total_bytes = 0;
    let mut observed = Vec::new();
    let mut unavailable = 0usize;
    for _ in 0..(MAX_THREADS * 2) {
        if remaining == 0 {
            break;
        }
        if Instant::now() >= deadline {
            bail!("status read deadline exceeded");
        }
        let message = ws.read().context("status read timed out")?;
        let Message::Text(text) = message else {
            continue;
        };
        total_bytes += text.len();
        if total_bytes > 8 * MAX_MESSAGE {
            bail!("status response budget exceeded");
        }
        // Deserialization discards previews, turns and other content, never retaining/exporting them.
        let Ok(reply) = serde_json::from_str::<Reply>(&text) else {
            continue;
        };
        let Some(index) = reply.id.and_then(|n| n.checked_sub(1)).map(|n| n as usize) else {
            continue;
        };
        let Some(expected) = ids.get(index) else {
            continue;
        };
        if !seen.insert(index) {
            continue;
        }
        remaining -= 1;
        let Some(thread) = reply.result.and_then(|r| r.thread) else {
            unavailable += 1;
            continue;
        };
        if &thread.id != expected {
            unavailable += 1;
            continue;
        }
        observed.push(thread);
    }
    if remaining != 0 {
        bail!("incomplete status response");
    }
    if observed.is_empty() && unavailable > 0 {
        bail!("status unavailable for {unavailable} thread(s)");
    }
    for thread in observed {
        apply(snapshot, thread);
    }
    if unavailable > 0 {
        snapshot.warnings.push(format!(
            "Codex live status unavailable for {unavailable} thread(s); showing log evidence"
        ));
    }
    // Closing this observer never unloads threads: thread/read did not subscribe.
    Ok(())
}

fn apply(snapshot: &mut Snapshot, thread: Thread) {
    let Some(status) = direct_status(thread.status, chrono::Utc::now().timestamp()) else {
        return;
    };
    for session in &mut snapshot.sessions {
        if !is_live_codex(session) || session.id != thread.id {
            continue;
        }
        apply_direct(session, &status);
    }
}

fn thread_ids(snapshot: &Snapshot) -> Vec<String> {
    snapshot
        .sessions
        .iter()
        .filter(|session| is_live_codex(session))
        .take(MAX_THREADS)
        .map(|session| session.id.clone())
        .collect()
}

fn watch_targets(snapshot: &Snapshot) -> Vec<WatchTarget> {
    snapshot
        .sessions
        .iter()
        .filter_map(|session| {
            if !is_live_codex(session) {
                return None;
            }
            Some(WatchTarget {
                id: session.id.clone(),
                pid: session.pid?,
                process_started_at: session.process_started_at?,
            })
        })
        .take(MAX_THREADS)
        .collect()
}

fn is_live_codex(session: &Session) -> bool {
    session.provider == Provider::Codex
        && session.pid.is_some()
        && session.insights.log_path.is_some()
}

fn direct_status(status: Status, observed_at: i64) -> Option<DirectStatus> {
    let (activity, event) = match status {
        Status::Idle => (Activity::Idle, "idle"),
        Status::Active { flags }
            if flags
                .iter()
                .any(|flag| flag == "waitingOnApproval" || flag == "waitingOnUserInput") =>
        {
            (Activity::WaitingInput, "waiting_for_input")
        }
        Status::Active { flags } if flags.is_empty() => (Activity::Working, "active"),
        Status::SystemError => (Activity::Unknown, "system_error"),
        // notLoaded describes this server, not whether a different embedded server is running.
        _ => return None,
    };
    Some(DirectStatus {
        activity,
        event,
        observed_at,
    })
}

fn apply_direct(session: &mut Session, status: &DirectStatus) {
    session.activity = status.activity.clone();
    session.confidence = Confidence::Observed;
    session.insights.activity_observation = Some(ActivityObservation {
        source: "codex_app_server".into(),
        event: status.event.into(),
        observed_at: status.observed_at,
    });
    session.evidence.push_str(
        "; runtime state from local Codex app-server (read-only observer; no thread attachment)",
    );
}

// Nonblocking connect and per-I/O deadlines keep a wedged daemon from stalling collection.
fn connect(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // Collector commands must not inherit this observer connection.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        bail!("invalid socket path");
    }
    for (dst, src) in address.sun_path.iter_mut().zip(bytes) {
        *dst = *src as _;
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    let rc = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as _,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error.into());
        }
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let milliseconds = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(1500) as i32;
        if unsafe { libc::poll(&mut poll, 1, milliseconds) } <= 0 {
            bail!("socket connection timed out");
        }
        if let Some(error) = stream.take_error()? {
            return Err(error.into());
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}
struct DeadlineStream {
    stream: UnixStream,
    deadline: Instant,
}
impl DeadlineStream {
    fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "observer deadline"))
    }
}
impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(buf)
    }
}
impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixListener, sync::mpsc};
    fn snapshot(ids: &[&str]) -> Snapshot {
        let mut snapshot = Snapshot::new("fixture".into());
        for id in ids {
            let mut session: crate::model::Session = serde_json::from_value(json!({
                "id":id,"provider":"codex","parent_id":null,"host":"fixture","pid":10,
                "process_started_at":100,"tty":null,"cwd":null,"model":null,
                "activity":"unknown","confidence":"inferred","evidence":"fixture","updated_at":null,"target":null
            })).unwrap();
            session.insights.log_path = Some(PathBuf::from("synthetic.jsonl"));
            snapshot.sessions.push(session);
        }
        snapshot
    }

    fn wait_for(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for watcher state"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn accept_observer(
        listener: &UnixListener,
    ) -> tungstenite::WebSocket<std::os::unix::net::UnixStream> {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        let init: serde_json::Value =
            serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["capabilities"]["experimentalApi"], false);
        let opted_out = init["params"]["capabilities"]["optOutNotificationMethods"]
            .as_array()
            .unwrap();
        assert!(
            !opted_out
                .iter()
                .any(|method| method == "thread/status/changed")
        );
        ws.send(Message::text(json!({"id":0,"result":{}}).to_string()))
            .unwrap();
        let ready: serde_json::Value =
            serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(ready["method"], "initialized");
        ws
    }

    fn read_thread_request(
        ws: &mut tungstenite::WebSocket<std::os::unix::net::UnixStream>,
    ) -> serde_json::Value {
        let read: serde_json::Value =
            serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(read["method"], "thread/read");
        assert_eq!(read["params"]["includeTurns"], false);
        read
    }

    #[test]
    fn watcher_handles_bootstrap_delta_disconnect_reconnect_and_unknown_threads() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (disconnect_tx, disconnect_rx) = mpsc::channel();
        let (unknown_tx, unknown_rx) = mpsc::channel();
        let (unknown_sent_tx, unknown_sent_rx) = mpsc::channel();
        let (delta_tx, delta_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut first = accept_observer(&listener);
            let read = read_thread_request(&mut first);
            // A transition during bootstrap must win over a stale read response.
            first
                .send(Message::text(
                    json!({"method":"thread/status/changed","params":{
                        "threadId":"tracked","status":{"type":"active","activeFlags":[]}
                    }})
                    .to_string(),
                ))
                .unwrap();
            first
                .send(Message::text(
                    json!({"id":read["id"],"result":{"thread":{
                        "id":"tracked","status":{"type":"idle"},
                        "preview":"SECRET_BOOTSTRAP_PREVIEW","turns":[]
                    }}})
                    .to_string(),
                ))
                .unwrap();
            disconnect_rx.recv().unwrap();
            drop(first);

            let mut second = accept_observer(&listener);
            let read = read_thread_request(&mut second);
            second
                .send(Message::text(
                    json!({"id":read["id"],"result":{"thread":{
                        "id":"tracked","status":{"type":"idle"}
                    }}})
                    .to_string(),
                ))
                .unwrap();
            unknown_rx.recv().unwrap();
            second
                .send(Message::text(
                    json!({"method":"thread/status/changed","params":{
                        "threadId":"not-tracked","status":{"type":"active","activeFlags":[]},
                        "preview":"SECRET_UNKNOWN_PREVIEW"
                    }})
                    .to_string(),
                ))
                .unwrap();
            unknown_sent_tx.send(()).unwrap();
            delta_rx.recv().unwrap();
            // Several read timeouts must leave this same WebSocket connected.
            std::thread::sleep(WATCH_IO_SLICE * 3);
            second
                .send(Message::text(
                    json!({"method":"thread/status/changed","params":{
                        "threadId":"tracked","status":{
                            "type":"active","activeFlags":["waitingOnUserInput"]
                        }
                    }})
                    .to_string(),
                ))
                .unwrap();
            finish_rx.recv().unwrap();
        });

        let watcher = Watcher::spawn_at(Some(socket));
        let raw = snapshot(&["tracked"]);
        watcher.update_targets(&raw);
        wait_for(|| {
            let mut current = raw.clone();
            watcher.apply(&mut current);
            current.sessions[0].activity == Activity::Working
        });
        assert!(watcher.take_changed());

        disconnect_tx.send(()).unwrap();
        wait_for(|| {
            let mut current = raw.clone();
            watcher.apply(&mut current);
            current.sessions[0].activity == Activity::Unknown
        });
        wait_for(|| {
            let mut current = raw.clone();
            watcher.apply(&mut current);
            current.sessions[0].activity == Activity::Idle
        });
        let _ = watcher.take_changed();

        unknown_tx.send(()).unwrap();
        unknown_sent_rx.recv().unwrap();
        std::thread::sleep(WATCH_IO_SLICE * 2);
        assert!(!watcher.take_changed());
        let mut current = raw.clone();
        watcher.apply(&mut current);
        assert_eq!(current.sessions[0].activity, Activity::Idle);
        assert!(!serde_json::to_string(&current).unwrap().contains("SECRET"));

        delta_tx.send(()).unwrap();
        wait_for(|| {
            let mut current = raw.clone();
            watcher.apply(&mut current);
            current.sessions[0].activity == Activity::WaitingInput
        });
        assert!(watcher.take_changed());
        finish_tx.send(()).unwrap();
        drop(watcher);
        server.join().unwrap();
    }

    #[test]
    fn watcher_binds_status_to_process_identity_and_preserves_observation_time() {
        let watcher = Watcher::spawn_at(None);
        let original = snapshot(&["tracked"]);
        watcher.update_targets(&original);
        {
            let mut state = watcher
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.statuses.insert(
                "tracked".into(),
                DirectStatus {
                    activity: Activity::Working,
                    event: "active",
                    observed_at: 123,
                },
            );
        }
        let mut applied = original.clone();
        watcher.apply(&mut applied);
        assert_eq!(applied.sessions[0].activity, Activity::Working);
        assert_eq!(
            applied.sessions[0]
                .insights
                .activity_observation
                .as_ref()
                .unwrap()
                .observed_at,
            123
        );

        let mut replaced = original.clone();
        replaced.sessions[0].process_started_at = Some(101);
        watcher.update_targets(&replaced);
        watcher.apply(&mut replaced);
        assert_eq!(replaced.sessions[0].activity, Activity::Unknown);
        assert!(replaced.sessions[0].insights.activity_observation.is_none());
    }

    #[test]
    fn observes_status_without_subscribing_resuming_or_retaining_content() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let init: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(init["method"], "initialize");
            assert_eq!(init["params"]["capabilities"]["experimentalApi"], false);
            ws.send(Message::text(json!({"id":0,"result":{}}).to_string()))
                .unwrap();
            let ready: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(ready["method"], "initialized");
            let statuses = [
                json!({"type":"active","activeFlags":[]}),
                json!({"type":"idle"}),
                json!({"type":"active","activeFlags":["waitingOnUserInput"]}),
                json!({"type":"notLoaded"}),
                json!({"type":"active","activeFlags":["futureFlag"]}),
            ];
            for status in statuses {
                let read: serde_json::Value =
                    serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(read["method"], "thread/read");
                assert_eq!(read["params"]["includeTurns"], false);
                ws.send(Message::text(
                    json!({"id":read["id"],"result":{"thread":{
                        "id":read["params"]["threadId"],"status":status,
                        "preview":"SECRET_FIXTURE_CONTENT", "turns":[]
                    }}})
                    .to_string(),
                ))
                .unwrap();
            }
            assert!(
                ws.read().is_err(),
                "observer must disconnect, not send another command"
            );
        });
        let mut data = snapshot(&["working", "ready", "input", "elsewhere", "future"]);
        enrich_at(&mut data, &socket).unwrap();
        assert_eq!(
            data.sessions
                .iter()
                .map(|s| s.activity.clone())
                .collect::<Vec<_>>(),
            vec![
                Activity::Working,
                Activity::Idle,
                Activity::WaitingInput,
                Activity::Unknown,
                Activity::Unknown
            ]
        );
        assert_eq!(data.sessions[0].confidence, Confidence::Observed);
        assert_eq!(data.sessions[3].confidence, Confidence::Inferred);
        assert!(
            !serde_json::to_string(&data)
                .unwrap()
                .contains("SECRET_FIXTURE_CONTENT")
        );
        server.join().unwrap();
    }

    #[test]
    fn stalled_server_is_bounded_and_does_not_invent_state() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(1800));
            drop(stream);
        });
        let mut data = snapshot(&["working"]);
        let start = Instant::now();
        assert!(enrich_at(&mut data, &socket).is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(data.sessions[0].activity, Activity::Unknown);
        server.join().unwrap();
    }

    #[test]
    fn incomplete_responses_discard_already_received_statuses() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let _: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::text(json!({"id":0,"result":{}}).to_string()))
                .unwrap();
            let _: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            let first: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            let _: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::text(
                json!({"id":first["id"],"result":{"thread":{
                    "id":first["params"]["threadId"],
                    "status":{"type":"active","activeFlags":[]}
                }}})
                .to_string(),
            ))
            .unwrap();
            // Disconnect before the second response. The first status must not
            // leak into a snapshot whose live-status pass was incomplete.
        });
        let mut data = snapshot(&["first", "second"]);
        assert!(enrich_at(&mut data, &socket).is_err());
        assert!(
            data.sessions
                .iter()
                .all(|session| session.activity == Activity::Unknown)
        );
        assert!(
            data.sessions
                .iter()
                .all(|session| session.insights.activity_observation.is_none())
        );
        server.join().unwrap();
    }

    #[test]
    fn all_rejected_reads_return_a_bounded_generic_error() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let _: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::text(json!({"id":0,"result":{}}).to_string()))
                .unwrap();
            let _: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            for _ in 0..2 {
                let read: serde_json::Value =
                    serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
                ws.send(Message::text(
                    json!({"id":read["id"],"error":{
                        "code":-32000,"message":"SECRET_SERVER_DETAIL"
                    }})
                    .to_string(),
                ))
                .unwrap();
            }
            assert!(
                ws.read().is_err(),
                "observer must disconnect after rejected replies"
            );
        });
        let mut data = snapshot(&["first", "second"]);
        let error = enrich_at(&mut data, &socket).unwrap_err().to_string();
        assert_eq!(error, "status unavailable for 2 thread(s)");
        assert!(!error.contains("SECRET_SERVER_DETAIL"));
        assert!(
            data.sessions
                .iter()
                .all(|session| session.activity == Activity::Unknown)
        );
        server.join().unwrap();
    }

    #[test]
    fn partial_rejections_keep_valid_status_and_add_only_a_count_warning() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let _: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::text(json!({"id":0,"result":{}}).to_string()))
                .unwrap();
            let _: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            let first: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            let second: serde_json::Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::text(
                json!({"id":first["id"],"result":{"thread":{
                    "id":first["params"]["threadId"],
                    "status":{"type":"active","activeFlags":[]}
                }}})
                .to_string(),
            ))
            .unwrap();
            ws.send(Message::text(
                json!({"id":second["id"],"result":{"thread":{
                    "id":"wrong-thread-id","status":{"type":"idle"},
                    "preview":"SECRET_REJECTED_PREVIEW"
                }}})
                .to_string(),
            ))
            .unwrap();
            assert!(
                ws.read().is_err(),
                "observer must disconnect after complete replies"
            );
        });
        let mut data = snapshot(&["first", "second"]);
        enrich_at(&mut data, &socket).unwrap();
        assert_eq!(data.sessions[0].activity, Activity::Working);
        assert_eq!(data.sessions[1].activity, Activity::Unknown);
        assert_eq!(
            data.warnings,
            vec!["Codex live status unavailable for 1 thread(s); showing log evidence"]
        );
        assert!(
            !serde_json::to_string(&data)
                .unwrap()
                .contains("SECRET_REJECTED_PREVIEW")
        );
        server.join().unwrap();
    }
}
