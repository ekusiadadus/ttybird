//! Manual, ephemeral capture of one UUID-addressed Ghostty terminal.
//! The AppleScript action writes no terminal input and does not focus the tab.

use anyhow::{Result, bail};

#[cfg(target_os = "macos")]
use anyhow::Context;

#[cfg(target_os = "macos")]
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::MetadataExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(target_os = "macos")]
use serde::Deserialize;

#[cfg(target_os = "macos")]
use crate::remote::run_bounded;

#[cfg(target_os = "macos")]
const SCRIPT: &str = include_str!("../scripts/ghostty_export.js");
#[cfg(target_os = "macos")]
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "macos")]
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "macos")]
const OUTPUT_LIMIT: usize = 16 * 1024;
#[cfg(target_os = "macos")]
const EXPORT_FILENAME: &str = "screen.txt";

#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
struct ExportReply {
    ok: bool,
    path: Option<String>,
    error: Option<String>,
}

/// Capture the current Ghostty screen plus retained scrollback as VT text.
///
/// This function is intended only for a user's explicit preview action. It
/// temporarily uses the general pasteboard because Ghostty 1.3.1 exposes the
/// generated file path only through `write_screen_file:copy,vt`.
#[cfg(target_os = "macos")]
pub fn capture(terminal_id: &str) -> Result<Vec<u8>> {
    validate_terminal_id(terminal_id)?;
    let reported_temporary_root = std::env::temp_dir();
    let temporary_root = reported_temporary_root
        .canonicalize()
        .context("Unable to resolve the private temporary directory")?;
    let reported_temporary_root_text = reported_temporary_root
        .to_str()
        .context("The private temporary directory is not valid Unicode")?;
    let temporary_root_text = temporary_root
        .to_str()
        .context("The private temporary directory is not valid Unicode")?;
    let _lock = ExportLock::acquire(&temporary_root)?;
    let args = vec![
        "-l".to_owned(),
        "JavaScript".to_owned(),
        "-e".to_owned(),
        SCRIPT.to_owned(),
        "--".to_owned(),
        terminal_id.to_owned(),
        reported_temporary_root_text.to_owned(),
        temporary_root_text.to_owned(),
    ];
    let output = run_bounded("osascript", &args, COMMAND_TIMEOUT, OUTPUT_LIMIT)
        .context("Ghostty snapshot export did not complete")?;
    let reply: ExportReply =
        serde_json::from_slice(&output).context("Ghostty snapshot export returned invalid data")?;

    if !reply.ok {
        if reply.error.as_deref() == Some("clipboard_restore_failed")
            && let Some(path) = reply.path.as_deref()
            && let Ok(path) =
                normalize_export_path(&reported_temporary_root, &temporary_root, Path::new(path))
        {
            cleanup_failed_export(&temporary_root, &path);
        }
        bail!(export_error(reply.error.as_deref()));
    }
    let path = reply
        .path
        .as_deref()
        .context("Ghostty snapshot export omitted its file path")?;
    let path = normalize_export_path(&reported_temporary_root, &temporary_root, Path::new(path))?;
    read_and_remove_export(&temporary_root, &path)
}

#[cfg(not(target_os = "macos"))]
pub fn capture(_terminal_id: &str) -> Result<Vec<u8>> {
    bail!("Ghostty screen export is available only on macOS")
}

#[cfg(any(target_os = "macos", test))]
fn validate_terminal_id(id: &str) -> Result<()> {
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

#[cfg(target_os = "macos")]
fn export_error(code: Option<&str>) -> &'static str {
    match code {
        Some("clipboard_too_large") => {
            "The clipboard is too large to preserve safely during Ghostty preview"
        }
        Some("clipboard_unavailable") => {
            "The clipboard cannot be preserved safely during Ghostty preview"
        }
        Some("clipboard_interference") => {
            "The clipboard changed during Ghostty preview; no snapshot was shown"
        }
        Some("clipboard_restore_failed") => {
            "The clipboard could not be restored after Ghostty preview"
        }
        Some("terminal_not_found") => "The selected Ghostty terminal no longer exists",
        Some("clipboard_unchanged") => "Ghostty did not publish a snapshot file path",
        Some("invalid_export_path") => "Ghostty returned an unsafe snapshot file path",
        Some("export_failed") => "Ghostty could not export the selected terminal",
        _ => "Ghostty snapshot export failed",
    }
}

#[cfg(target_os = "macos")]
fn export_directory_name<'a>(
    reported_temporary_root: &Path,
    temporary_root: &Path,
    path: &'a Path,
) -> Result<&'a Path> {
    if path.file_name().and_then(|value| value.to_str()) != Some(EXPORT_FILENAME) {
        bail!("Ghostty returned an unsafe snapshot file path");
    }
    let directory = path
        .parent()
        .context("Ghostty returned an unsafe snapshot file path")?;
    let directory_name = directory
        .file_name()
        .and_then(|value| value.to_str())
        .context("Ghostty returned an unsafe snapshot file path")?;
    if directory_name.len() != 22
        || !directory_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || !matches!(
            directory.parent(),
            Some(parent) if parent == reported_temporary_root || parent == temporary_root
        )
    {
        bail!("Ghostty returned an unsafe snapshot file path");
    }
    Ok(Path::new(directory_name))
}

#[cfg(target_os = "macos")]
fn normalize_export_path(
    reported_temporary_root: &Path,
    temporary_root: &Path,
    path: &Path,
) -> Result<PathBuf> {
    let directory_name = export_directory_name(reported_temporary_root, temporary_root, path)?;
    Ok(temporary_root.join(directory_name).join(EXPORT_FILENAME))
}

#[cfg(target_os = "macos")]
fn open_export(temporary_root: &Path, path: &Path) -> Result<(File, File, File, CString)> {
    let directory_name = export_directory_name(temporary_root, temporary_root, path)?;
    let root = File::open(temporary_root).context("Unable to inspect the snapshot directory")?;
    let directory_name = CString::new(
        directory_name
            .to_str()
            .expect("validated temporary directory name is ASCII"),
    )
    .expect("validated temporary directory name has no NUL");
    let directory_descriptor = unsafe {
        libc::openat(
            root.as_raw_fd(),
            directory_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if directory_descriptor == -1 {
        bail!("Unable to open the Ghostty snapshot directory safely");
    }
    let directory_file = unsafe { File::from_raw_fd(directory_descriptor) };
    let directory_metadata = directory_file
        .metadata()
        .context("Unable to inspect the snapshot directory")?;
    if !directory_metadata.is_dir() || directory_metadata.uid() != unsafe { libc::geteuid() } {
        bail!("Ghostty returned an unsafe snapshot directory");
    }

    let filename = CString::new(EXPORT_FILENAME).expect("static filename has no NUL");
    let descriptor = unsafe {
        libc::openat(
            directory_file.as_raw_fd(),
            filename.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor == -1 {
        bail!("Unable to open the Ghostty snapshot safely");
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file
        .metadata()
        .context("Unable to inspect the Ghostty snapshot")?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        bail!("Ghostty returned an unsafe snapshot file");
    }
    Ok((root, directory_file, file, directory_name))
}

#[cfg(target_os = "macos")]
fn read_and_remove_export(temporary_root: &Path, path: &Path) -> Result<Vec<u8>> {
    let (root, directory, mut file, directory_name) = open_export(temporary_root, path)?;
    let mut bytes = Vec::new();
    let read_result = file
        .by_ref()
        .take((MAX_SNAPSHOT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("Unable to read the Ghostty snapshot");
    drop(file);
    let cleanup_result = remove_export(&root, &directory, &directory_name);
    read_result?;
    cleanup_result?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        bail!("Ghostty snapshot exceeded the 1 MiB limit");
    }
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn remove_export(root: &File, directory: &File, directory_name: &CString) -> Result<()> {
    if unsafe { libc::unlinkat(directory.as_raw_fd(), c"screen.txt".as_ptr(), 0) } == -1 {
        bail!("Unable to remove the Ghostty snapshot");
    }
    if unsafe {
        libc::unlinkat(
            root.as_raw_fd(),
            directory_name.as_ptr(),
            libc::AT_REMOVEDIR,
        )
    } == -1
    {
        bail!("Unable to remove the Ghostty snapshot directory");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn cleanup_failed_export(temporary_root: &Path, path: &Path) {
    if let Ok((root, directory, file, directory_name)) = open_export(temporary_root, path) {
        drop(file);
        let _ = remove_export(&root, &directory, &directory_name);
    }
}

#[cfg(target_os = "macos")]
struct ExportLock(File);

#[cfg(target_os = "macos")]
impl ExportLock {
    fn acquire(temporary_root: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(temporary_root.join("ttybird-ghostty-export.lock"))
            .context("Unable to open the Ghostty export lock")?;
        let metadata = file
            .metadata()
            .context("Unable to inspect the Ghostty export lock")?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
        {
            bail!("The Ghostty export lock is unsafe");
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            bail!("Another TTYbird Ghostty preview is already running");
        }
        Ok(Self(file))
    }
}

#[cfg(target_os = "macos")]
impl Drop for ExportLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_only_canonical_terminal_ids() {
        assert!(validate_terminal_id("12345678-1234-1234-1234-123456789abc").is_ok());
        for value in [
            "12345678123412341234123456789abc",
            "12345678-1234-1234-1234-123456789abg",
            "12345678-1234-1234-1234-123456789abc/extra",
        ] {
            assert!(validate_terminal_id(value).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn accepts_only_the_pinned_ghostty_export_shape() {
        let root = Path::new("/private/tmp/example");
        let valid = root.join("AbCdEf0123456789_-wxyz").join(EXPORT_FILENAME);
        assert!(export_directory_name(root, root, &valid).is_ok());
        for invalid in [
            root.join("too-short").join(EXPORT_FILENAME),
            root.join("AbCdEf0123456789_-wxyz").join("screen.html"),
            root.join("AbCdEf0123456789_+wxyz").join(EXPORT_FILENAME),
            root.join("AbCdEf0123456789_-wxyz")
                .join("nested")
                .join(EXPORT_FILENAME),
        ] {
            assert!(export_directory_name(root, root, &invalid).is_err());
        }
        let reported_root = Path::new("/var/tmp/example");
        let reported = reported_root
            .join("AbCdEf0123456789_-wxyz")
            .join(EXPORT_FILENAME);
        assert_eq!(
            normalize_export_path(reported_root, root, &reported).unwrap(),
            valid
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_and_removes_only_an_owned_regular_export() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let directory = root.join("AbCdEf0123456789_-wxyz");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(EXPORT_FILENAME);
        std::fs::write(&path, b"snapshot\x1b[0m").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            read_and_remove_export(&root, &path).unwrap(),
            b"snapshot\x1b[0m"
        );
        assert!(!directory.exists());

        let target = root.join("unrelated.txt");
        std::fs::write(&target, b"keep").unwrap();
        let link_directory = root.join("ZbCdEf0123456789_-wxyz");
        std::fs::create_dir(&link_directory).unwrap();
        let link = link_directory.join(EXPORT_FILENAME);
        symlink(&target, &link).unwrap();
        assert!(read_and_remove_export(&root, &link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");

        let large_directory = root.join("YbCdEf0123456789_-wxyz");
        std::fs::create_dir(&large_directory).unwrap();
        let large = large_directory.join(EXPORT_FILENAME);
        std::fs::write(&large, vec![b'x'; MAX_SNAPSHOT_BYTES + 1]).unwrap();
        std::fs::set_permissions(&large, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_and_remove_export(&root, &large).is_err());
        assert!(!large_directory.exists());
    }
}
