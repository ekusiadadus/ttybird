//! Read-only runtime status from an already-running local Codex app-server.
//! No daemon launch, subscription, thread resume, model call, or approval reply.
use crate::model::{Activity, ActivityObservation, Confidence, Provider, Snapshot};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use std::{
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
    time::{Duration, Instant},
};
use tungstenite::{Message, client::client_with_config, protocol::WebSocketConfig};

const BUDGET: Duration = Duration::from_millis(1500);
const MAX_THREADS: usize = 64;
const MAX_MESSAGE: usize = 1024 * 1024;

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
#[derive(Deserialize)]
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
    let ids: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|s| {
            s.provider == Provider::Codex && s.pid.is_some() && s.insights.log_path.is_some()
        })
        .take(MAX_THREADS)
        .map(|s| s.id.clone())
        .collect();
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
    let (activity, event) = match thread.status {
        Status::Idle => (Activity::Idle, "idle"),
        Status::Active { flags }
            if flags
                .iter()
                .any(|f| f == "waitingOnApproval" || f == "waitingOnUserInput") =>
        {
            (Activity::WaitingInput, "waiting_for_input")
        }
        Status::Active { flags } if flags.is_empty() => (Activity::Working, "active"),
        Status::SystemError => (Activity::Unknown, "system_error"),
        // notLoaded describes this server, not whether a different embedded server is running.
        _ => return,
    };
    for session in &mut snapshot.sessions {
        if session.provider != Provider::Codex || session.id != thread.id || session.pid.is_none() {
            continue;
        }
        session.activity = activity.clone();
        session.confidence = Confidence::Observed;
        session.insights.activity_observation = Some(ActivityObservation {
            source: "codex_app_server".into(),
            event: event.into(),
            observed_at: chrono::Utc::now().timestamp(),
        });
        session
            .evidence
            .push_str("; runtime state read from local Codex app-server (no subscription)");
    }
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
    use std::os::unix::net::UnixListener;
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
