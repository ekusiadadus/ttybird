//! Owned terminal sessions that outlive the dashboard which launched them.
//!
//! Only process identity metadata is persisted. PTY output, command arguments,
//! and the child environment remain in the session daemon's memory.

use crate::managed_vt::{Engine, KeyInput, Screen};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, OsStr},
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[cfg(unix)]
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    io::{AsRawFd, FromRawFd, RawFd},
    net::{UnixListener, UnixStream},
    process::CommandExt,
};

const REQUEST_LIMIT: u64 = 64 * 1024;
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const INPUT_QUEUE_LIMIT: usize = 256 * 1024;
const MAX_COLS: u16 = 200;
const MAX_ROWS: u16 = 80;
const SOCKET_PATH_LIMIT: usize = 103;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const SERVER_CLIENT_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub program: String,
    pub cwd: PathBuf,
    pub pid: u32,
    pub process_started_at: u64,
    pub daemon_pid: u32,
    pub daemon_started_at: u64,
    pub tty: String,
    pub ended: bool,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Frame { cols: u16, rows: u16 },
    Key { key: KeyInput },
    Paste { text: String },
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Frame { screen: Screen, ended: bool },
    Ok,
    Error { message: String },
}

pub fn launch(config_dir: &Path, name: Option<&str>, command: &[String]) -> Result<SessionInfo> {
    #[cfg(not(unix))]
    {
        let _ = (config_dir, name, command);
        bail!("managed terminal sessions require Unix")
    }
    #[cfg(unix)]
    {
        validate_command(name, command)?;
        let dir = session_dir(config_dir)?;
        let id = new_id(&dir)?;
        let socket = socket_path(&dir, &id)?;
        let executable = std::env::current_exe().context("locate the ttybird executable")?;
        let mut daemon = Command::new(executable);
        daemon
            .arg("--config-dir")
            .arg(config_dir)
            .arg("__session-server")
            .arg(&id);
        if let Some(name) = name {
            daemon.arg("--name").arg(name);
        }
        daemon
            .arg("--")
            .args(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            daemon.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut daemon = daemon.spawn().context("start session server")?;
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Some(status) = daemon.try_wait().context("query session server")? {
                if let Ok(info) = read_info(&metadata_path(&dir, &id))
                    && info.ended
                {
                    return Ok(info);
                }
                bail!("session server exited during startup: {status}");
            }
            if socket.exists() {
                match raw_request(
                    &socket,
                    &Request::Paste {
                        text: String::new(),
                    },
                ) {
                    Ok(Response::Ok) => {
                        let info = read_info(&metadata_path(&dir, &id))?;
                        // Reap the daemon if this launcher remains alive (for
                        // example, while its dashboard is attached). Detached
                        // Rust threads do not extend process lifetime.
                        thread::spawn(move || {
                            let _ = daemon.wait();
                        });
                        return Ok(info);
                    }
                    Ok(Response::Error { message }) => {
                        let _ = stop_daemon_bounded(&mut daemon);
                        bail!("session command failed to start: {message}");
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            if Instant::now() >= deadline {
                let _ = stop_daemon_bounded(&mut daemon);
                bail!("session server did not become ready within 5 seconds");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(unix)]
fn stop_daemon_bounded(daemon: &mut Child) -> Result<()> {
    let pid = daemon.id() as i32;
    let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_millis(750);
    loop {
        if daemon.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
            daemon.wait()?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn serve(config_dir: &Path, id: &str, name: Option<&str>, command: &[String]) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (config_dir, id, name, command);
        bail!("managed terminal sessions require Unix")
    }
    #[cfg(unix)]
    {
        validate_id(id)?;
        validate_command(name, command)?;
        let termination = daemon_termination_flag()?;
        let dir = session_dir(config_dir)?;
        let socket = socket_path(&dir, id)?;
        let metadata = metadata_path(&dir, id);
        reject_existing_session(&socket, &metadata)?;

        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("bind session socket {}", socket.display()))?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let _socket_guard = SocketGuard(socket);
        listener.set_nonblocking(true)?;

        let (master, slave, tty) = open_pty(80, 24)?;
        let mut child = match spawn_in_pty(command, slave) {
            Ok(child) => child,
            Err(error) => {
                serve_startup_error(&listener, format!("{error:#}"));
                return Err(error);
            }
        };
        let setup = (|| -> Result<SessionInfo> {
            let daemon_pid = std::process::id();
            let child_pid = child.id();
            let daemon_started_at = wait_for_identity(daemon_pid)
                .context("could not establish session server identity")?;
            let process_started_at = wait_for_identity(child_pid)
                .context("could not establish child process identity")?;
            let info = SessionInfo {
                id: id.to_owned(),
                name: name.map(str::to_owned),
                program: program_name(&command[0]),
                cwd: std::env::current_dir().context("read session working directory")?,
                pid: child_pid,
                process_started_at,
                daemon_pid,
                daemon_started_at,
                tty,
                ended: false,
                exit_code: None,
            };
            write_info(&metadata, &info)?;
            Ok(info)
        })();
        let mut info = match setup {
            Ok(info) => info,
            Err(error) => {
                terminate_child(&mut child, master);
                return Err(error);
            }
        };

        let result = run_daemon(&listener, master, &mut child, &termination);
        if result.is_err() {
            terminate_process_group(&mut child);
        }
        let status = child
            .try_wait()
            .ok()
            .flatten()
            .or_else(|| child.wait().ok());
        info.ended = true;
        info.exit_code = status.and_then(|value| value.code());
        let metadata_result = write_info(&metadata, &info);
        result.and(metadata_result)
    }
}

pub fn list(config_dir: &Path) -> Result<Vec<SessionInfo>> {
    #[cfg(not(unix))]
    {
        let _ = config_dir;
        return Ok(Vec::new());
    }
    #[cfg(unix)]
    {
        let dir = config_dir.join("managed");
        match fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
            Ok(_) => validate_session_dir(&dir)?,
        }
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension() != Some(OsStr::new("json")) {
                continue;
            }
            let Ok(info) = read_info(&path) else { continue };
            if validate_id(&info.id).is_err() || metadata_path(&dir, &info.id) != path {
                continue;
            }
            if !info.ended
                && (crate::collect::process_identity(info.daemon_pid)
                    != Some(info.daemon_started_at)
                    || crate::collect::process_identity(info.pid) != Some(info.process_started_at))
            {
                continue;
            }
            sessions.push(info);
        }
        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(sessions)
    }
}

pub fn request(config_dir: &Path, id: &str, request: Request) -> Result<Response> {
    #[cfg(not(unix))]
    {
        let _ = (config_dir, id, request);
        bail!("managed terminal sessions require Unix")
    }
    #[cfg(unix)]
    {
        validate_id(id)?;
        let dir = config_dir.join("managed");
        validate_session_dir(&dir)?;
        let info = read_info(&metadata_path(&dir, id))?;
        ensure!(info.id == id, "session metadata identity mismatch");
        ensure!(!info.ended, "session has ended");
        ensure!(
            crate::collect::process_identity(info.daemon_pid) == Some(info.daemon_started_at),
            "session server identity is stale"
        );
        ensure!(
            crate::collect::process_identity(info.pid) == Some(info.process_started_at),
            "session child identity is stale"
        );
        let socket = socket_path(&dir, id)?;
        validate_socket(&socket)?;
        raw_request(&socket, &request)
            .with_context(|| format!("exchange request with session {id}"))
    }
}

fn validate_command(name: Option<&str>, command: &[String]) -> Result<()> {
    ensure!(!command.is_empty(), "a session command is required");
    ensure!(!command[0].is_empty(), "session program cannot be empty");
    if let Some(name) = name {
        ensure!(!name.is_empty(), "session name cannot be empty");
        ensure!(name.len() <= 256, "session name is too long");
        ensure!(
            !name.chars().any(char::is_control),
            "session name cannot contain control characters"
        );
    }
    Ok(())
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 64, "invalid session id");
    ensure!(
        id.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid session id"
    );
    Ok(())
}

#[cfg(unix)]
fn session_dir(config_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(config_dir)
        .with_context(|| format!("create config directory {}", config_dir.display()))?;
    let dir = config_dir.join("managed");
    match fs::symlink_metadata(&dir) {
        Ok(metadata) => {
            validate_session_dir_metadata(&metadata)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(&dir)?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(dir)
}

#[cfg(unix)]
fn validate_session_dir(dir: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(dir)
        .with_context(|| format!("read managed session directory {}", dir.display()))?;
    validate_session_dir_metadata(&metadata)
}

#[cfg(unix)]
fn validate_session_dir_metadata(metadata: &fs::Metadata) -> Result<()> {
    ensure!(
        metadata.file_type().is_dir(),
        "managed session path is not a directory"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "managed session directory has a different owner"
    );
    ensure!(
        metadata.mode() & 0o077 == 0,
        "managed session directory permissions must be 0700"
    );
    Ok(())
}

#[cfg(unix)]
fn socket_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    let path = dir.join(format!("{id}.sock"));
    ensure!(
        path.as_os_str().as_bytes().len() <= SOCKET_PATH_LIMIT,
        "config path is too long for a portable Unix socket"
    );
    Ok(path)
}

fn metadata_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

#[cfg(unix)]
fn new_id(dir: &Path) -> Result<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock predates Unix epoch")?
        .as_nanos();
    for suffix in 0..100u8 {
        let id = format!("{:x}-{:x}-{suffix:x}", std::process::id(), nanos);
        if !metadata_path(dir, &id).exists() && !socket_path(dir, &id)?.exists() {
            return Ok(id);
        }
    }
    bail!("could not allocate a unique session id")
}

fn program_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(program)
        .to_owned()
}

#[cfg(unix)]
fn reject_existing_session(socket: &Path, metadata: &Path) -> Result<()> {
    match fs::symlink_metadata(metadata) {
        Ok(_) => {
            let info = read_info(metadata)?;
            if !info.ended
                && crate::collect::process_identity(info.daemon_pid) == Some(info.daemon_started_at)
            {
                bail!("session server already exists");
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::symlink_metadata(socket) {
        Ok(value) => {
            ensure!(
                value.file_type().is_socket(),
                "session socket path is not a socket"
            );
            ensure!(
                value.uid() == unsafe { libc::geteuid() },
                "session socket has a different owner"
            );
            fs::remove_file(socket)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn validate_socket(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("session socket is unavailable")?;
    ensure!(
        metadata.file_type().is_socket(),
        "session socket path is not a socket"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "session socket has a different owner"
    );
    ensure!(
        metadata.mode() & 0o077 == 0,
        "session socket permissions are not private"
    );
    Ok(())
}

#[cfg(unix)]
fn raw_request(socket: &Path, request: &Request) -> Result<Response> {
    validate_socket(socket)?;
    let data = serde_json::to_vec(request)?;
    ensure!(
        data.len() <= REQUEST_LIMIT as usize,
        "session request is too large"
    );
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(&data)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let response =
        read_bounded(&mut stream, RESPONSE_LIMIT as u64).context("read session response")?;
    serde_json::from_slice(&response).context("invalid session response")
}

#[cfg(unix)]
fn read_info(path: &Path) -> Result<SessionInfo> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read session metadata {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "session metadata is not a regular file"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "session metadata has a different owner"
    );
    ensure!(
        metadata.mode() & 0o077 == 0,
        "session metadata permissions are not private"
    );
    ensure!(
        metadata.nlink() == 1,
        "session metadata has unexpected hard links"
    );
    let data = fs::read(path)?;
    ensure!(
        data.len() <= REQUEST_LIMIT as usize,
        "session metadata is too large"
    );
    serde_json::from_slice(&data).context("invalid session metadata")
}

#[cfg(unix)]
fn write_info(path: &Path, info: &SessionInfo) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "session metadata is not a regular file"
            );
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() },
                "session metadata has a different owner"
            );
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let data = serde_json::to_vec_pretty(info)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    let result = (|| -> Result<()> {
        let mut file = options.open(&tmp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(tmp);
    result
}

#[cfg(unix)]
fn daemon_termination_flag() -> Result<Arc<AtomicBool>> {
    static FLAG: OnceLock<std::result::Result<Arc<AtomicBool>, String>> = OnceLock::new();
    match FLAG.get_or_init(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let handler_flag = Arc::clone(&flag);
        ctrlc::set_handler(move || handler_flag.store(true, Ordering::SeqCst))
            .map(|()| flag)
            .map_err(|error| error.to_string())
    }) {
        Ok(flag) => Ok(Arc::clone(flag)),
        Err(error) => bail!("install session server termination handler: {error}"),
    }
}

#[cfg(unix)]
fn wait_for_identity(pid: u32) -> Option<u64> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(started_at) = observed_process_start(pid) {
            return Some(started_at);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn observed_process_start(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system
        .process(pid)
        .map(|process| process.start_time())
        .filter(|started_at| *started_at > 0)
}

#[cfg(unix)]
fn open_pty(cols: u16, rows: u16) -> Result<(File, File, String)> {
    let mut master = -1;
    let mut slave = -1;
    let mut dimensions = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // Darwin declares this mutable; Linux declares it const.
            &raw mut dimensions,
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error()).context("open PTY");
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    set_fd_flag(
        master.as_raw_fd(),
        libc::F_GETFL,
        libc::F_SETFL,
        libc::O_NONBLOCK,
    )?;
    set_fd_flag(
        master.as_raw_fd(),
        libc::F_GETFD,
        libc::F_SETFD,
        libc::FD_CLOEXEC,
    )?;
    set_fd_flag(
        slave.as_raw_fd(),
        libc::F_GETFD,
        libc::F_SETFD,
        libc::FD_CLOEXEC,
    )?;
    let tty = tty_name(slave.as_raw_fd())?;
    Ok((master, slave, tty))
}

#[cfg(unix)]
fn set_fd_flag(fd: RawFd, get: libc::c_int, set: libc::c_int, flag: libc::c_int) -> Result<()> {
    let old = unsafe { libc::fcntl(fd, get) };
    if old == -1 || unsafe { libc::fcntl(fd, set, old | flag) } == -1 {
        return Err(std::io::Error::last_os_error()).context("configure PTY descriptor");
    }
    Ok(())
}

#[cfg(unix)]
fn tty_name(fd: RawFd) -> Result<String> {
    let mut buffer = vec![0i8; 1024];
    let status = unsafe { libc::ttyname_r(fd, buffer.as_mut_ptr(), buffer.len()) };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status)).context("read PTY name");
    }
    let value = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    Ok(value.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn spawn_in_pty(command: &[String], slave: File) -> Result<Child> {
    let stdin = slave.try_clone()?;
    let stdout = slave.try_clone()?;
    let mut child = Command::new(&command[0]);
    child
        .args(&command[1..])
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    // A managed child owns a new PTY, so inherited terminal-emulator and
    // multiplexer identity must not claim it still belongs to the caller.
    for (key, _) in std::env::vars_os() {
        if is_terminal_identity_env(&key) {
            child.env_remove(key);
        }
    }
    unsafe {
        child.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    child.spawn().context("spawn session command")
}

fn is_terminal_identity_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key == "TMUX"
        || key == "TMUX_PANE"
        || key == "TERM_PROGRAM"
        || key == "TERM_PROGRAM_VERSION"
        || key == "COLORTERM"
        || key == "WT_SESSION"
        || key == "KONSOLE_VERSION"
        || key.starts_with("GHOSTTY_")
        || key.starts_with("KITTY_")
        || key.starts_with("WEZTERM_")
}

#[cfg(unix)]
fn serve_startup_error(listener: &UnixListener, message: String) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = write_response(&mut stream, &Response::Error { message });
                return;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return,
        }
    }
}

#[cfg(unix)]
fn run_daemon(
    listener: &UnixListener,
    mut master: File,
    child: &mut Child,
    termination: &AtomicBool,
) -> Result<()> {
    let mut engine = Engine::new(80, 24)?;
    let mut dimensions = (80, 24);
    let mut input = Vec::<u8>::new();
    let mut input_offset = 0usize;
    loop {
        if termination.load(Ordering::SeqCst) {
            terminate_child(child, master);
            return Ok(());
        }
        if child.try_wait().context("query session child")?.is_some() {
            drain_master(&mut master, &mut engine, &mut input, &mut input_offset)?;
            return Ok(());
        }
        let pending = input.len().saturating_sub(input_offset);
        let mut descriptors = [
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN | if pending > 0 { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 100) };
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("poll session PTY");
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            drain_master(&mut master, &mut engine, &mut input, &mut input_offset)?;
        }
        if descriptors[0].revents & libc::POLLOUT != 0 {
            flush_input(&mut master, &mut input, &mut input_offset)?;
        }
        if descriptors[1].revents & libc::POLLIN != 0 {
            for _ in 0..16 {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error).context("accept session request"),
                };
                let stop = match handle_client(
                    &mut stream,
                    &mut master,
                    &mut engine,
                    &mut dimensions,
                    &mut input,
                    &mut input_offset,
                ) {
                    Ok(stop) => stop,
                    Err(_) => continue,
                };
                if stop {
                    terminate_child(child, master);
                    return Ok(());
                }
            }
        }
        if descriptors[0].revents & libc::POLLHUP != 0 {
            drain_master(&mut master, &mut engine, &mut input, &mut input_offset)?;
            if child
                .try_wait()
                .context("query hung-up session child")?
                .is_none()
            {
                // There is no terminal to manage after every slave descriptor
                // closes. Do not spin on a permanently readable POLLHUP or
                // leave a detached child without its owned terminal.
                terminate_process_group(child);
            }
            return Ok(());
        }
        if descriptors[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            bail!("session PTY failed");
        }
    }
}

#[cfg(unix)]
fn handle_client(
    stream: &mut UnixStream,
    master: &mut File,
    engine: &mut Engine,
    dimensions: &mut (u16, u16),
    input: &mut Vec<u8>,
    input_offset: &mut usize,
) -> Result<bool> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(SERVER_CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_CLIENT_TIMEOUT))?;
    let request = match read_bounded(stream, REQUEST_LIMIT)
        .and_then(|data| serde_json::from_slice::<Request>(&data).map_err(Into::into))
    {
        Ok(request) => request,
        Err(error) => {
            let _ = write_response(
                stream,
                &Response::Error {
                    message: format!("invalid request: {error}"),
                },
            );
            return Ok(false);
        }
    };
    if matches!(request, Request::Stop) {
        write_response(stream, &Response::Ok)?;
        return Ok(true);
    }
    let response = (|| -> Result<Response> {
        Ok(match request {
            Request::Frame { cols, rows } => {
                if cols == 0 || rows == 0 || cols > MAX_COLS || rows > MAX_ROWS {
                    Response::Error {
                        message: "terminal dimensions are out of range".into(),
                    }
                } else {
                    if *dimensions != (cols, rows) {
                        resize_pty(master.as_raw_fd(), cols, rows)?;
                        engine.resize(cols, rows)?;
                        *dimensions = (cols, rows);
                    }
                    drain_master(master, engine, input, input_offset)?;
                    Response::Frame {
                        screen: engine.snapshot()?,
                        ended: false,
                    }
                }
            }
            Request::Key { key } => match engine.encode_key(&key) {
                Ok(bytes) => queue_input(input, input_offset, bytes).map_or_else(
                    |error| Response::Error {
                        message: error.to_string(),
                    },
                    |_| Response::Ok,
                ),
                Err(error) => Response::Error {
                    message: error.to_string(),
                },
            },
            Request::Paste { text } => match engine.encode_paste(&text) {
                Ok(bytes) => queue_input(input, input_offset, bytes).map_or_else(
                    |error| Response::Error {
                        message: error.to_string(),
                    },
                    |_| Response::Ok,
                ),
                Err(error) => Response::Error {
                    message: error.to_string(),
                },
            },
            Request::Stop => unreachable!("stop was handled above"),
        })
    })()
    .unwrap_or_else(|error| Response::Error {
        message: error.to_string(),
    });
    if let Err(error) = flush_input(master, input, input_offset) {
        write_response(
            stream,
            &Response::Error {
                message: error.to_string(),
            },
        )?;
        return Ok(false);
    }
    write_response(stream, &response)?;
    Ok(false)
}

#[cfg(unix)]
fn drain_master(
    master: &mut File,
    engine: &mut Engine,
    input: &mut Vec<u8>,
    input_offset: &mut usize,
) -> Result<()> {
    let mut buffer = [0u8; 32 * 1024];
    for _ in 0..16 {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                engine.feed(&buffer[..count])?;
                let replies = engine.replies();
                queue_input(input, input_offset, replies)?;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) if is_pty_eof(&error) => break,
            Err(error) => return Err(error).context("read session PTY"),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn queue_input(input: &mut Vec<u8>, offset: &mut usize, bytes: Vec<u8>) -> Result<()> {
    ensure!(
        input.len().saturating_sub(*offset) + bytes.len() <= INPUT_QUEUE_LIMIT,
        "session input queue is full"
    );
    if *offset == input.len() {
        input.clear();
        *offset = 0;
    } else if *offset > 64 * 1024 {
        input.drain(..*offset);
        *offset = 0;
    }
    input.extend_from_slice(&bytes);
    Ok(())
}

#[cfg(unix)]
fn flush_input(master: &mut File, input: &mut Vec<u8>, offset: &mut usize) -> Result<()> {
    while *offset < input.len() {
        match master.write(&input[*offset..]) {
            Ok(0) => break,
            Ok(count) => *offset += count,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) if is_pty_eof(&error) => break,
            Err(error) => return Err(error).context("write session PTY"),
        }
    }
    if *offset == input.len() {
        input.clear();
        *offset = 0;
    }
    Ok(())
}

#[cfg(unix)]
fn resize_pty(fd: RawFd, cols: u16, rows: u16) -> Result<()> {
    let dimensions = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &dimensions) } == -1 {
        return Err(std::io::Error::last_os_error()).context("resize session PTY");
    }
    Ok(())
}

#[cfg(unix)]
fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let data = serde_json::to_vec(response)?;
    ensure!(
        data.len() <= RESPONSE_LIMIT,
        "session response is too large"
    );
    stream.write_all(&data)?;
    Ok(())
}

fn read_bounded(reader: &mut impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(limit + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "message exceeds size limit");
    Ok(data)
}

#[cfg(unix)]
fn terminate_child(child: &mut Child, master: File) {
    drop(master);
    terminate_process_group(child);
}

#[cfg(unix)]
fn terminate_process_group(child: &mut Child) {
    let process_group = child.id() as i32;
    let _ = unsafe { libc::kill(-process_group, libc::SIGTERM) };
    // Do not reap the group leader during the grace period: keeping it as a
    // zombie prevents its PID/process-group ID from being reused before
    // descendants that ignored SIGTERM receive the final group-wide SIGKILL.
    thread::sleep(Duration::from_millis(100));
    let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    let _ = child.wait();
}

#[cfg(unix)]
fn is_pty_eof(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EIO | libc::ENXIO))
}

#[cfg(unix)]
struct SocketGuard(PathBuf);

#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_session_ids() {
        assert!(validate_id("abc-123_DEF").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("../escape").is_err());
        assert!(validate_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn stores_only_program_basename() {
        assert_eq!(program_name("/usr/local/bin/codex"), "codex");
        assert_eq!(program_name("claude"), "claude");
    }

    #[test]
    fn bounded_reader_rejects_oversize_messages() {
        assert!(read_bounded(&mut &b"1234"[..], 3).is_err());
        assert_eq!(read_bounded(&mut &b"123"[..], 3).unwrap(), b"123");
    }

    #[cfg(unix)]
    #[test]
    fn metadata_permissions_are_private() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let managed = session_dir(directory.path()).unwrap();
        let path = metadata_path(&managed, "fixture");
        let info = SessionInfo {
            id: "fixture".into(),
            name: Some("test".into()),
            program: "sh".into(),
            cwd: PathBuf::from("/tmp"),
            pid: 1,
            process_started_at: 1,
            daemon_pid: 1,
            daemon_started_at: 1,
            tty: "/dev/pts/1".into(),
            ended: true,
            exit_code: Some(0),
        };
        write_info(&path, &info).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_info(&path).unwrap(), info);
    }

    #[cfg(unix)]
    #[test]
    fn list_does_not_create_session_directory() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        assert!(list(directory.path()).unwrap().is_empty());
        assert!(!directory.path().join("managed").exists());
    }

    #[cfg(unix)]
    #[test]
    fn pty_round_trip_and_stop_cleanup() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let root = directory.path().to_owned();
        let server_root = root.clone();
        let id = "integration";
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf 'ready:%s' \"$TERM\"; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done".to_owned(),
        ];
        let server = thread::spawn(move || serve(&server_root, id, None, &command));
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while !root.join("managed/integration.json").exists() {
            assert!(
                Instant::now() < deadline,
                "session metadata was not created"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let mut saw_ready = false;
        for _ in 0..20 {
            let Response::Frame { screen, .. } =
                request(&root, id, Request::Frame { cols: 80, rows: 24 }).unwrap()
            else {
                panic!("expected a terminal frame");
            };
            if screen.vt.contains("ready:xterm-256color") {
                saw_ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_ready, "initial PTY output was not captured");
        assert!(matches!(
            request(
                &root,
                id,
                Request::Frame {
                    cols: MAX_COLS + 1,
                    rows: 24
                }
            )
            .unwrap(),
            Response::Error { .. }
        ));
        assert!(matches!(
            request(
                &root,
                id,
                Request::Paste {
                    text: "hello\n".into()
                }
            )
            .unwrap(),
            Response::Ok
        ));

        let mut saw_echo = false;
        for _ in 0..20 {
            let Response::Frame { screen, .. } =
                request(&root, id, Request::Frame { cols: 80, rows: 24 }).unwrap()
            else {
                panic!("expected a terminal frame");
            };
            if screen.vt.contains("got:hello") {
                saw_echo = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_echo, "PTY input did not reach the child");
        assert!(matches!(
            request(&root, id, Request::Stop).unwrap(),
            Response::Ok
        ));
        server.join().unwrap().unwrap();
        assert!(!root.join("managed/integration.sock").exists());
        let ended = read_info(&root.join("managed/integration.json")).unwrap();
        assert!(ended.ended);
    }

    #[cfg(unix)]
    #[test]
    fn natural_exit_retains_only_ended_metadata() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let root = directory.path().to_owned();
        let server_root = root.clone();
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 0.15; exit 7".to_owned(),
        ];
        let server = thread::spawn(move || serve(&server_root, "natural", None, &command));
        server.join().unwrap().unwrap();
        assert!(!root.join("managed/natural.sock").exists());
        let info = read_info(&root.join("managed/natural.json")).unwrap();
        assert!(info.ended);
        assert_eq!(info.exit_code, Some(7));
    }

    #[cfg(unix)]
    #[test]
    fn stop_kills_descendant_that_ignores_sigterm() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let pid_file = directory.path().join("descendant.pid");
        let (master, slave, _) = open_pty(80, 24).unwrap();
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "( trap '' TERM; while :; do sleep 1; done ) & echo $! > \"$1\"; wait".to_owned(),
            "fixture".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        ];
        let mut child = spawn_in_pty(&command, slave).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let descendant = loop {
            if let Ok(value) = fs::read_to_string(&pid_file)
                && let Ok(pid) = value.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "descendant PID was not written");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(crate::collect::process_identity(descendant).is_some());

        terminate_child(&mut child, master);
        let deadline = Instant::now() + Duration::from_secs(2);
        while crate::collect::process_identity(descendant).is_some() {
            assert!(
                Instant::now() < deadline,
                "SIGTERM-ignoring descendant survived group cleanup"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
