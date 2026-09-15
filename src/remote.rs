use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::model::{PROTOCOL_VERSION, Snapshot};

const SNAPSHOT_LIMIT: usize = 2 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const PIPE_CHUNK: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

enum PipeEvent {
    Data(Stream, Vec<u8>),
    Eof(Stream),
    Error(Stream, std::io::Error),
}

#[cfg(unix)]
fn isolate_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn isolate_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn kill_process_tree(child: &mut std::process::Child) {
    if let Ok(process_group) = i32::try_from(child.id()) {
        // SAFETY: the child was placed in a new process group whose id equals
        // its pid. A negative pid addresses only that group, and SIGKILL has no
        // borrowed memory or lifetime requirements.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
    // The child may have moved itself into another process group. Killing it
    // directly ensures wait() can still reap the process; independently
    // nonblocking readers handle any detached descendant retaining a pipe.
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_process_tree(child: &mut std::process::Child) {
    let _ = child.kill();
}

#[cfg(unix)]
fn make_nonblocking<R: std::os::fd::AsRawFd>(reader: &R) -> std::io::Result<()> {
    let descriptor = reader.as_raw_fd();
    // SAFETY: fcntl receives a valid descriptor borrowed for each call.
    // F_GETFL has no third argument; F_SETFL retains no pointer.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn read_pipe_loop<R: Read>(
    mut reader: R,
    stream: Stream,
    sender: SyncSender<PipeEvent>,
    stop: Arc<AtomicBool>,
) {
    let mut buffer = vec![0_u8; PIPE_CHUNK];
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(PipeEvent::Eof(stream));
                return;
            }
            Ok(count) => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if sender
                    .send(PipeEvent::Data(stream, buffer[..count].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => {
                let _ = sender.send(PipeEvent::Error(stream, error));
                return;
            }
        }
    }
}

#[cfg(unix)]
fn read_pipe<R: Read + Send + std::os::fd::AsRawFd + 'static>(
    reader: R,
    stream: Stream,
    sender: SyncSender<PipeEvent>,
    stop: Arc<AtomicBool>,
) {
    if let Err(error) = make_nonblocking(&reader) {
        let _ = sender.send(PipeEvent::Error(stream, error));
        return;
    }
    read_pipe_loop(reader, stream, sender, stop);
}

#[cfg(not(unix))]
fn read_pipe<R: Read + Send + 'static>(
    reader: R,
    stream: Stream,
    sender: SyncSender<PipeEvent>,
    stop: Arc<AtomicBool>,
) {
    read_pipe_loop(reader, stream, sender, stop);
}

/// Run a subprocess while bounding its lifetime and captured output.
///
/// Only stdout is returned. Stderr is drained concurrently, capped, and never
/// included in errors because commands may put prompts or sensitive values there.
pub fn run_bounded(
    program: &str,
    args: &[String],
    timeout: Duration,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    if program.is_empty() {
        bail!("program must not be empty");
    }
    if timeout.is_zero() {
        bail!("timeout must be greater than zero");
    }
    if max_bytes == 0 {
        bail!("output limit must be greater than zero");
    }

    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_process_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;

    let stdout = child
        .stdout
        .take()
        .context("child stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("child stderr was not captured")?;
    let (sender, receiver) = mpsc::sync_channel(16);
    let stop = Arc::new(AtomicBool::new(false));
    let stdout_sender = sender.clone();
    let stdout_stop = Arc::clone(&stop);
    let stderr_stop = Arc::clone(&stop);
    let stdout_reader =
        thread::spawn(move || read_pipe(stdout, Stream::Stdout, stdout_sender, stdout_stop));
    let stderr_reader =
        thread::spawn(move || read_pipe(stderr, Stream::Stderr, sender, stderr_stop));

    let deadline = Instant::now() + timeout;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = 0_usize;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut status = None;
    let mut failure = None;

    while failure.is_none() && !(status.is_some() && stdout_eof && stderr_eof) {
        if Instant::now() >= deadline {
            failure = Some(anyhow!(
                "process timed out after {} ms",
                timeout.as_millis()
            ));
            break;
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(child_status) => status = child_status,
                Err(error) => {
                    failure = Some(anyhow!("failed to query child process: {error}"));
                    continue;
                }
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(10));
        match receiver.recv_timeout(wait) {
            Ok(PipeEvent::Data(Stream::Stdout, bytes)) => {
                if stdout_bytes.len().saturating_add(bytes.len()) > max_bytes {
                    failure = Some(anyhow!("process stdout exceeded {max_bytes} bytes"));
                } else {
                    stdout_bytes.extend_from_slice(&bytes);
                }
            }
            Ok(PipeEvent::Data(Stream::Stderr, bytes)) => {
                stderr_bytes = stderr_bytes.saturating_add(bytes.len());
                if stderr_bytes > STDERR_LIMIT {
                    failure = Some(anyhow!("process stderr exceeded {STDERR_LIMIT} bytes"));
                }
            }
            Ok(PipeEvent::Eof(Stream::Stdout)) => stdout_eof = true,
            Ok(PipeEvent::Eof(Stream::Stderr)) => stderr_eof = true,
            Ok(PipeEvent::Error(stream, error)) => {
                failure = Some(anyhow!("failed reading child {stream:?}: {error}"));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                stdout_eof = true;
                stderr_eof = true;
                if status.is_none() {
                    thread::sleep(wait);
                }
            }
        }
    }

    // A reader may be blocked while sending into the bounded channel when a
    // timeout or size limit makes us stop receiving. Disconnect it first so
    // both reader threads can observe the shutdown and exit.
    stop.store(true, Ordering::Release);
    drop(receiver);
    if failure.is_some() {
        kill_process_tree(&mut child);
    }
    let final_status = child.wait();

    // Waiting above closes the ordinary SSH/osascript pipes. Joining prevents
    // detached reader threads from accumulating across repeated collections.
    stdout_reader
        .join()
        .map_err(|_| anyhow!("stdout reader thread panicked"))?;
    stderr_reader
        .join()
        .map_err(|_| anyhow!("stderr reader thread panicked"))?;
    let final_status = final_status.context("failed to reap child process")?;

    if let Some(error) = failure {
        return Err(error);
    }
    if !final_status.success() {
        bail!("process exited unsuccessfully ({final_status})");
    }
    Ok(stdout_bytes)
}

fn valid_chars(value: &str, allow_slash: bool) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.len() <= 4096
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'@' | b'-')
                || (allow_slash && byte == b'/')
        })
}

pub fn validate_host(host: &str) -> Result<()> {
    if !valid_chars(host, false) {
        bail!("remote host must be a safe SSH alias");
    }
    Ok(())
}

pub fn validate_binary(binary: &str) -> Result<()> {
    if !valid_chars(binary, true) {
        bail!("remote binary must be a safe path");
    }
    Ok(())
}

fn validate_remote(host: &str, binary: &str) -> Result<()> {
    validate_host(host)?;
    validate_binary(binary)
}

fn collect_with_ssh(
    ssh_program: &str,
    host: &str,
    binary: &str,
    timeout: Duration,
) -> Result<Snapshot> {
    validate_remote(host, binary)?;
    let connect_seconds = timeout.as_secs().clamp(1, 60);
    let args = vec![
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        format!("ConnectTimeout={connect_seconds}"),
        host.to_owned(),
        format!("{binary} collect --json"),
    ];
    let output = run_bounded(ssh_program, &args, timeout, SNAPSHOT_LIMIT)
        .with_context(|| format!("remote collection failed for {host}"))?;
    let snapshot: Snapshot =
        serde_json::from_slice(&output).context("remote returned invalid snapshot JSON")?;
    if snapshot.protocol_version != PROTOCOL_VERSION {
        bail!(
            "unsupported remote protocol version {}; expected {}",
            snapshot.protocol_version,
            PROTOCOL_VERSION
        );
    }
    Ok(snapshot)
}

pub fn collect(host: &str, binary: &str, timeout: Duration) -> Result<Snapshot> {
    collect_with_ssh("ssh", host, binary, timeout)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    fn script(contents: &str) -> (TempDir, String) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fake-command");
        fs::write(&path, format!("#!/bin/sh\n{contents}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        (temp, path.to_string_lossy().into_owned())
    }

    #[test]
    fn validates_remote_identifiers() {
        assert!(validate_remote("build@example.test", "/opt/bin/ttybird").is_ok());
        assert!(validate_remote("-oProxyCommand=bad", "ttybird").is_err());
        assert!(validate_remote("host name", "ttybird").is_err());
        assert!(validate_remote("host", "ttybird;bad").is_err());
        assert!(validate_remote("host", "-ttybird").is_err());
    }

    #[test]
    fn bounded_runner_stops_at_stdout_limit() {
        let (_temp, path) = script("printf '123456789'");
        // This tests the byte limit, not process startup latency under build
        // load. The separate timeout test keeps its 50 ms deadline.
        let error = run_bounded(&path, &[], Duration::from_secs(5), 8).unwrap_err();
        assert!(
            error.to_string().contains("stdout exceeded"),
            "expected stdout limit failure, got: {error:#}"
        );
    }

    #[test]
    fn bounded_runner_stops_infinite_output_without_channel_deadlock() {
        for (redirect, expected) in [("", "stdout exceeded"), (" >&2", "stderr exceeded")] {
            let (_temp, path) = script(&format!("while :; do printf '123456789'{redirect}; done"));
            let started = Instant::now();
            let error = run_bounded(&path, &[], Duration::from_secs(5), 1024).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
            assert!(started.elapsed() < Duration::from_secs(4));
        }
    }

    #[test]
    fn bounded_runner_kills_and_reaps_on_timeout() {
        let (_temp, path) = script("sleep 5");
        let started = Instant::now();
        let error = run_bounded(&path, &[], Duration::from_millis(50), 32).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_runner_does_not_wait_for_detached_pipe_holder() {
        let python_available = Command::new("python3")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        assert!(
            python_available,
            "python3 is required for detached-pipe lifecycle coverage"
        );

        let contents = r#"python3 -c 'import os,sys,time; os.setsid(); p=open(sys.argv[1],"w"); p.write(str(os.getpid())); p.close(); time.sleep(10)' "$1" &
while [ ! -s "$1" ]; do sleep 0.01; done
printf '123456789'
exit 0"#;
        let (temp, path) = script(contents);
        let pid_file = temp.path().join("detached.pid");
        let args = vec![pid_file.to_string_lossy().into_owned()];
        let started = Instant::now();
        let error = run_bounded(&path, &args, Duration::from_secs(5), 8).unwrap_err();
        let elapsed = started.elapsed();

        let mut detached_pid = None;
        for _ in 0..50 {
            if let Ok(value) = fs::read_to_string(&pid_file) {
                detached_pid = value.parse::<i32>().ok();
                if detached_pid.is_some() {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(pid) = detached_pid {
            // SAFETY: this exact pid was written by the test-owned detached
            // fixture immediately after setsid, and the fixture sleeps until
            // well after this cleanup executes.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }

        assert!(error.to_string().contains("stdout exceeded"));
        assert!(elapsed < Duration::from_secs(3));
        assert!(
            detached_pid.is_some(),
            "detached fixture did not report its pid"
        );
    }

    #[test]
    fn collect_enforces_snapshot_protocol() {
        for version in [1, 99] {
            let json = format!(
                r#"{{"protocol_version":{version},"host":"worker","collected_at":1,"sessions":[],"warnings":[]}}"#
            );
            let (_temp, path) = script(&format!("printf '%s' '{json}'"));
            let result = collect_with_ssh(&path, "worker", "ttybird", Duration::from_secs(5));
            if version == PROTOCOL_VERSION {
                let snapshot = result.unwrap();
                assert_eq!(snapshot.host, "worker");
                assert_eq!(snapshot.protocol_version, PROTOCOL_VERSION);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("unsupported remote protocol")
                );
            }
        }
    }
}
