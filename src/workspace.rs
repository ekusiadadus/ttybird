use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{model::Session, remote::run_bounded};

const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_OUTPUT_LIMIT: usize = 512 * 1024;
const METADATA_FILE_LIMIT: u64 = 4096;
const MAX_CHANGES: usize = 32;

/// Read-only Git metadata for one checkout.
///
/// `checkout_root` identifies a working tree. `git_common_dir` identifies the
/// repository shared by linked worktrees, so callers can relate checkouts
/// without merging their branch, HEAD, or dirty state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub checkout_root: PathBuf,
    pub git_common_dir: PathBuf,
    /// `None` for a detached HEAD. An unborn branch still has a branch name.
    pub branch: Option<String>,
    /// Full object ID, or `None` when HEAD is unborn.
    pub head_commit: Option<String>,
    pub dirty: bool,
    pub changes: Vec<WorkspaceChange>,
    pub changes_truncated: bool,
    pub linked_worktree: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceChange {
    /// Checkout-relative path. Non-UTF-8 bytes are represented lossily.
    pub path: String,
    pub kind: WorkspaceChangeKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Conflicted,
    Untracked,
    Other,
}

#[derive(Debug, Clone)]
struct CheckoutLocation {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    linked_worktree: bool,
}

/// Per-collection cache. Each canonical checkout is queried at most once.
///
/// Construct a fresh value for each collection so an older dirty/HEAD state is
/// never retained across refreshes.
#[derive(Debug, Default)]
pub struct Inspector {
    by_checkout: HashMap<PathBuf, WorkspaceInfo>,
}

impl Inspector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inspect(&mut self, path: &Path) -> Result<WorkspaceInfo> {
        let location = locate_checkout(path)?;
        if let Some(info) = self.by_checkout.get(&location.root) {
            return Ok(info.clone());
        }

        let info = inspect_location(location)?;
        self.by_checkout
            .insert(info.checkout_root.clone(), info.clone());
        Ok(info)
    }
}

/// Inspect one path without retaining a cross-call cache.
pub fn inspect(path: &Path) -> Result<WorkspaceInfo> {
    Inspector::new().inspect(path)
}

/// Group distinct checkouts that share Git's common repository directory.
///
/// Duplicate observations of the same checkout are collapsed. This describes
/// repository topology only; it does not imply that any session is editing.
pub fn group_checkouts_by_common_dir<'a>(
    infos: impl IntoIterator<Item = &'a WorkspaceInfo>,
) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let mut groups = BTreeMap::<PathBuf, Vec<PathBuf>>::new();
    for info in infos {
        let roots = groups.entry(info.git_common_dir.clone()).or_default();
        if !roots.contains(&info.checkout_root) {
            roots.push(info.checkout_root.clone());
        }
    }
    for roots in groups.values_mut() {
        roots.sort();
    }
    groups
}

/// Count distinct live top-level sessions observed in each checkout.
///
/// Parent/child rows count once. Named roots use their session identity because
/// one app-server process may host independent root tasks; raw rows fall back
/// to PID/start identity. A raw process row is omitted when a named root session
/// of the same provider and host is already associated with that checkout.
/// Counts describe observed session placement, not authorship or simultaneous
/// editing.
pub fn live_root_counts(sessions: &[Session]) -> HashMap<PathBuf, usize> {
    let candidates: Vec<_> = sessions
        .iter()
        .filter(|session| {
            session.parent_id.is_none()
                && session.pid.is_some()
                && session.process_started_at.is_some()
                && session.insights.workspace.is_some()
        })
        .collect();
    let mut named_identities = HashSet::new();
    let mut raw_identities = HashSet::new();
    let mut counts = HashMap::new();

    for session in &candidates {
        let workspace = session.insights.workspace.as_ref().expect("filtered above");
        if is_raw_process(session)
            && candidates.iter().any(|other| {
                !is_raw_process(other)
                    && other.host == session.host
                    && other.provider == session.provider
                    && other.pid == session.pid
                    && other.process_started_at == session.process_started_at
                    && other
                        .insights
                        .workspace
                        .as_ref()
                        .is_some_and(|info| info.checkout_root == workspace.checkout_root)
            })
        {
            continue;
        }

        let unique = if is_raw_process(session) {
            raw_identities.insert((
                session.host.clone(),
                session.provider,
                session.pid.expect("filtered above"),
                session.process_started_at.expect("filtered above"),
            ))
        } else {
            named_identities.insert((session.host.clone(), session.provider, session.id.clone()))
        };
        if unique {
            *counts.entry(workspace.checkout_root.clone()).or_insert(0) += 1;
        }
    }
    counts
}

fn is_raw_process(session: &Session) -> bool {
    match (session.pid, session.process_started_at) {
        (Some(pid), Some(started_at)) => {
            session.id == format!("{}-pid-{pid}-{started_at}", session.provider.as_str())
        }
        _ => false,
    }
}

fn inspect_location(location: CheckoutLocation) -> Result<WorkspaceInfo> {
    let root = location
        .root
        .to_str()
        .context("Git checkout path is not valid UTF-8")?;
    let git_dir = location
        .git_dir
        .to_str()
        .context("Git metadata path is not valid UTF-8")?;
    location
        .common_dir
        .to_str()
        .context("Git common directory path is not valid UTF-8")?;
    let args = vec![
        "--no-optional-locks".to_owned(),
        "-c".to_owned(),
        "core.fsmonitor=false".to_owned(),
        "-c".to_owned(),
        "core.untrackedCache=false".to_owned(),
        "-c".to_owned(),
        "submodule.recurse=false".to_owned(),
        "-c".to_owned(),
        "status.submoduleSummary=false".to_owned(),
        "--git-dir".to_owned(),
        git_dir.to_owned(),
        "--work-tree".to_owned(),
        root.to_owned(),
        "status".to_owned(),
        "--porcelain=v2".to_owned(),
        "--branch".to_owned(),
        "-z".to_owned(),
        "--untracked-files=normal".to_owned(),
    ];
    let output = run_bounded("git", &args, GIT_TIMEOUT, GIT_OUTPUT_LIMIT)
        .context("failed to inspect Git checkout")?;
    let status = parse_status(&output)?;

    Ok(WorkspaceInfo {
        checkout_root: location.root,
        git_common_dir: location.common_dir,
        branch: status.branch,
        head_commit: status.head_commit,
        dirty: status.dirty,
        changes: status.changes,
        changes_truncated: status.changes_truncated,
        linked_worktree: location.linked_worktree,
    })
}

fn locate_checkout(path: &Path) -> Result<CheckoutLocation> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("could not resolve workspace path {}", path.display()))?;
    let start = if canonical.is_dir() {
        canonical.as_path()
    } else {
        canonical
            .parent()
            .context("workspace path has no parent directory")?
    };

    let root = start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .context("path is not inside a Git checkout")?
        .to_path_buf();
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
            .canonicalize()
            .context("could not resolve Git metadata directory")?
    } else {
        let metadata = read_small_text(&dot_git)?;
        let value = metadata
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("gitdir:"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("invalid .git indirection file")?;
        let target = Path::new(value);
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            root.join(target)
        };
        target
            .canonicalize()
            .context("could not resolve linked worktree Git directory")?
    };

    let commondir_file = git_dir.join("commondir");
    let (common_dir, linked_worktree) = if commondir_file.is_file() {
        let value = read_small_text(&commondir_file)?;
        let value = value.trim();
        if value.is_empty() {
            bail!("Git commondir file is empty");
        }
        let target = Path::new(value);
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            git_dir.join(target)
        };
        (
            target
                .canonicalize()
                .context("could not resolve Git common directory")?,
            true,
        )
    } else {
        (git_dir.clone(), false)
    };

    Ok(CheckoutLocation {
        root,
        git_dir,
        common_dir,
        linked_worktree,
    })
}

fn read_small_text(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("could not read Git metadata file {}", path.display()))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(METADATA_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > METADATA_FILE_LIMIT {
        bail!("Git metadata file exceeds {METADATA_FILE_LIMIT} bytes");
    }
    String::from_utf8(bytes).context("Git metadata file is not valid UTF-8")
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedStatus {
    branch: Option<String>,
    head_commit: Option<String>,
    dirty: bool,
    changes: Vec<WorkspaceChange>,
    changes_truncated: bool,
}

fn parse_status(output: &[u8]) -> Result<ParsedStatus> {
    let mut branch_seen = false;
    let mut oid_seen = false;
    let mut branch = None;
    let mut head_commit = None;
    let mut dirty = false;
    let mut changes = Vec::new();
    let mut changes_truncated = false;
    let mut records = output.split(|byte| *byte == 0);

    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            oid_seen = true;
            if value != b"(initial)" {
                if !(40..=64).contains(&value.len()) || !value.iter().all(u8::is_ascii_hexdigit) {
                    bail!("Git returned an invalid HEAD object ID");
                }
                head_commit = Some(String::from_utf8(value.to_vec())?);
            }
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.head ") {
            branch_seen = true;
            if value != b"(detached)" && value != b"(unknown)" {
                branch = Some(String::from_utf8_lossy(value).into_owned());
            }
            continue;
        }
        if record.starts_with(b"# ") {
            continue;
        }

        let (path, kind, consumes_original_path) = parse_change_record(record)?;
        if consumes_original_path && records.next().is_none() {
            bail!("Git returned an incomplete rename record");
        }
        dirty = true;
        if changes.len() < MAX_CHANGES {
            changes.push(WorkspaceChange {
                path: String::from_utf8_lossy(path).into_owned(),
                kind,
            });
        } else {
            changes_truncated = true;
        }
    }

    if !branch_seen || !oid_seen {
        bail!("Git status omitted HEAD metadata");
    }
    Ok(ParsedStatus {
        branch,
        head_commit,
        dirty,
        changes,
        changes_truncated,
    })
}

fn parse_change_record(record: &[u8]) -> Result<(&[u8], WorkspaceChangeKind, bool)> {
    match record.first().copied() {
        Some(b'?') if record.starts_with(b"? ") => {
            Ok((&record[2..], WorkspaceChangeKind::Untracked, false))
        }
        Some(b'u') if record.starts_with(b"u ") => {
            let path = nth_field_tail(record, 10)?;
            Ok((path, WorkspaceChangeKind::Conflicted, false))
        }
        Some(b'1') if record.starts_with(b"1 ") => {
            let xy = nth_field(record, 1)?;
            let path = nth_field_tail(record, 8)?;
            Ok((path, kind_from_xy(xy, None), false))
        }
        Some(b'2') if record.starts_with(b"2 ") => {
            let xy = nth_field(record, 1)?;
            let score = nth_field(record, 8)?;
            let path = nth_field_tail(record, 9)?;
            Ok((path, kind_from_xy(xy, score.first().copied()), true))
        }
        _ => bail!("Git returned an unknown status record"),
    }
}

fn nth_field(record: &[u8], index: usize) -> Result<&[u8]> {
    record
        .split(|byte| *byte == b' ')
        .nth(index)
        .context("Git returned a malformed status record")
}

fn nth_field_tail(record: &[u8], index: usize) -> Result<&[u8]> {
    record
        .splitn(index + 1, |byte| *byte == b' ')
        .nth(index)
        .filter(|value| !value.is_empty())
        .context("Git returned a status record without a path")
}

fn kind_from_xy(xy: &[u8], rename_or_copy: Option<u8>) -> WorkspaceChangeKind {
    if xy.contains(&b'U') || matches!(xy, b"AA" | b"DD") {
        WorkspaceChangeKind::Conflicted
    } else if rename_or_copy == Some(b'R') {
        WorkspaceChangeKind::Renamed
    } else if rename_or_copy == Some(b'C') {
        WorkspaceChangeKind::Copied
    } else if xy.contains(&b'D') {
        WorkspaceChangeKind::Deleted
    } else if xy.contains(&b'A') {
        WorkspaceChangeKind::Added
    } else if xy.contains(&b'T') {
        WorkspaceChangeKind::TypeChanged
    } else if xy.contains(&b'M') {
        WorkspaceChangeKind::Modified
    } else {
        WorkspaceChangeKind::Other
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::TempDir;

    use super::*;
    use crate::model::{Activity, Confidence, Provider, SessionInsights};

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn repository() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        git(&root, &["init"]);
        git(&root, &["config", "user.name", "TTYbird Test"]);
        git(&root, &["config", "user.email", "ttybird@example.invalid"]);
        fs::write(root.join("tracked.txt"), "initial\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "initial"]);
        git(&root, &["branch", "-M", "main"]);
        (temp, root)
    }

    fn observed_session(id: &str, pid: u32, workspace: WorkspaceInfo) -> Session {
        Session {
            id: id.to_owned(),
            provider: Provider::Codex,
            parent_id: None,
            host: "local".to_owned(),
            pid: Some(pid),
            process_started_at: Some(100 + u64::from(pid)),
            tty: None,
            cwd: Some(workspace.checkout_root.to_string_lossy().into_owned()),
            model: None,
            activity: Activity::Unknown,
            confidence: Confidence::Unknown,
            evidence: "fixture".to_owned(),
            updated_at: None,
            target: None,
            insights: SessionInsights {
                workspace: Some(workspace),
                ..SessionInsights::default()
            },
        }
    }

    #[test]
    fn inspects_branch_dirty_paths_subdirectories_and_symlinks_with_one_cache() {
        let (temp, root) = repository();
        let subdir = root.join("nested");
        fs::create_dir(&subdir).unwrap();
        let alias = temp.path().join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&subdir, &alias).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&subdir, &alias).unwrap();

        let mut inspector = Inspector::new();
        let clean = inspector.inspect(&subdir).unwrap();
        assert_eq!(clean.checkout_root, root.canonicalize().unwrap());
        assert_eq!(clean.branch.as_deref(), Some("main"));
        let expected_head = git(&root, &["rev-parse", "HEAD"]);
        assert_eq!(clean.head_commit.as_deref(), Some(expected_head.as_str()));
        assert!(!clean.dirty);
        assert!(!clean.linked_worktree);

        fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        fs::write(root.join("untracked.txt"), "new\n").unwrap();

        // A collection reuses one checkout observation even through another cwd.
        assert!(!inspector.inspect(&alias).unwrap().dirty);

        let dirty = inspect(&alias).unwrap();
        assert!(dirty.dirty);
        assert!(dirty.changes.contains(&WorkspaceChange {
            path: "tracked.txt".to_owned(),
            kind: WorkspaceChangeKind::Modified,
        }));
        assert!(dirty.changes.contains(&WorkspaceChange {
            path: "untracked.txt".to_owned(),
            kind: WorkspaceChangeKind::Untracked,
        }));
        assert!(!dirty.changes_truncated);
    }

    #[test]
    fn keeps_linked_worktrees_distinct_while_grouping_their_common_repository() {
        let (temp, root) = repository();
        let linked = temp.path().join("linked");
        git(
            &root,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );

        let main = inspect(&root).unwrap();
        let feature = inspect(&linked).unwrap();
        assert_ne!(main.checkout_root, feature.checkout_root);
        assert_eq!(main.git_common_dir, feature.git_common_dir);
        assert!(!main.linked_worktree);
        assert!(feature.linked_worktree);
        assert_eq!(feature.branch.as_deref(), Some("feature"));

        git(&linked, &["checkout", "--detach"]);
        let detached = inspect(&linked).unwrap();
        assert_eq!(detached.branch, None);
        assert_eq!(detached.head_commit, feature.head_commit);

        let groups = group_checkouts_by_common_dir([&main, &feature, &feature]);
        assert_eq!(groups.len(), 1);
        let roots = groups.values().next().unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&main.checkout_root));
        assert!(roots.contains(&feature.checkout_root));

        let first = observed_session("logical-1", 1, main.clone());
        let mut child = observed_session("child", 1, main.clone());
        child.parent_id = Some(first.id.clone());
        let second_same_pid = observed_session("logical-2", 1, main.clone());
        let duplicate_record = observed_session("logical-1", 1, main.clone());
        let represented_raw = observed_session("codex-pid-1-101", 1, main.clone());
        let independent_raw = observed_session("codex-pid-2-102", 2, main.clone());
        let linked_session = observed_session("logical-linked", 4, feature.clone());
        let counts = live_root_counts(&[
            first,
            child,
            second_same_pid,
            duplicate_record,
            represented_raw,
            independent_raw,
            linked_session,
        ]);
        // The exact raw duplicate of PID 1 is suppressed. The independent
        // unmatched PID 2 in the same checkout remains a distinct live root.
        assert_eq!(counts.get(&main.checkout_root), Some(&3));
        assert_eq!(counts.get(&feature.checkout_root), Some(&1));
    }

    #[test]
    fn bounds_the_changed_path_list() {
        let (_temp, root) = repository();
        for index in 0..(MAX_CHANGES + 3) {
            fs::write(root.join(format!("untracked-{index:02}.txt")), "new\n").unwrap();
        }
        let info = inspect(&root).unwrap();
        assert!(info.dirty);
        assert_eq!(info.changes.len(), MAX_CHANGES);
        assert!(info.changes_truncated);
    }
}
