//! Durable, evidence-bounded attention events and optional desktop notifications.
//!
//! This module never sends input to an agent. It only consumes allowlisted
//! Claude hook evidence already present in collected snapshots.

use crate::{
    config,
    model::{Activity, Confidence, Provider, Session, Snapshot},
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const STATE_VERSION: u32 = 1;
const STALE_AFTER_SECONDS: i64 = 5 * 60;
const RETAIN_SECONDS: i64 = 7 * 24 * 60 * 60;
const MAX_ITEMS: usize = 1024;
const MAX_NOTIFICATIONS_PER_RUN: usize = 2;
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    PermissionRequest,
    InputRequest,
    ResponseFinished,
    ToolError,
}

impl AttentionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::PermissionRequest => "Permission requested",
            Self::InputRequest => "Input requested",
            Self::ResponseFinished => "Response finished",
            Self::ToolError => "Tool failed",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionStatus {
    Open,
    Acknowledged,
    Resolved,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttentionItem {
    pub id: String,
    pub kind: AttentionKind,
    pub status: AttentionStatus,
    pub host: String,
    pub provider: Provider,
    pub session_id: String,
    pub pid: u32,
    pub process_started_at: u64,
    pub occurrence: u64,
    pub title: Option<String>,
    pub observed_at: i64,
    pub opened_at: i64,
    pub last_seen_at: i64,
    pub read_at: Option<i64>,
    pub acknowledged_at: Option<i64>,
    pub snoozed_until: Option<i64>,
    pub notified_at: Option<i64>,
    #[serde(default)]
    next_notify_at: Option<i64>,
    #[serde(default)]
    notify_failures: u8,
}

impl AttentionItem {
    pub fn is_open(&self) -> bool {
        self.status == AttentionStatus::Open
    }

    pub fn is_snoozed(&self, now: i64) -> bool {
        self.snoozed_until.is_some_and(|until| until > now)
    }

    pub fn wants_notification(&self, now: i64) -> bool {
        self.is_open()
            && !self.is_snoozed(now)
            && self.read_at.is_none()
            && self.notified_at.is_none()
            && self.next_notify_at.is_none_or(|retry| retry <= now)
    }
}

#[derive(Debug, Clone)]
pub struct InboxUpdate {
    /// Most recent first. Resolved and expired entries are retained briefly so
    /// the UI can explain why an item disappeared from the active list.
    pub items: Vec<AttentionItem>,
    /// IDs are advisory. `notify_pending` rechecks them under the file guard.
    pub notification_candidates: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationReport {
    pub sent: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionKey {
    host: String,
    provider: Provider,
    session_id: String,
    pid: u32,
    process_started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Track {
    key: SessionKey,
    occurrence: u64,
    last_observed_at: i64,
    last_event: String,
    active_item_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    tracks: Vec<Track>,
    #[serde(default)]
    items: Vec<AttentionItem>,
}

pub struct AttentionInbox {
    config_dir: PathBuf,
}

impl AttentionInbox {
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
        }
    }

    /// Fold a complete collection cycle into the durable inbox. Snapshot
    /// refreshes of one sustained hook event keep the same occurrence.
    pub fn update(&self, snapshots: &[Snapshot], now: i64) -> Result<InboxUpdate> {
        let Some(_guard) = StateGuard::try_acquire(&self.config_dir, Duration::from_millis(50))?
        else {
            // Another TTYbird may be completing a bounded notification send.
            // Atomic replacement makes this lock-free read safe; retain the
            // last durable metadata and fold the new snapshot on a later tick.
            let mut state = load_state(&self.config_dir)?;
            expire_and_bound(&mut state, now);
            return Ok(inbox_update(&state, now));
        };
        let mut state = load_state(&self.config_dir)?;
        let before = serde_json::to_vec(&state)?;
        for snapshot in snapshots {
            for session in &snapshot.sessions {
                apply_session(&mut state, session, now);
            }
        }
        expire_and_bound(&mut state, now);
        save_if_changed(&self.config_dir, before, &mut state)?;
        Ok(inbox_update(&state, now))
    }

    pub fn items(&self, now: i64) -> Result<Vec<AttentionItem>> {
        self.mutate(|state| {
            expire_and_bound(state, now);
            Ok(sorted_items(state))
        })
    }

    pub fn mark_all_read(&self, now: i64) -> Result<()> {
        self.mutate(|state| {
            for item in &mut state.items {
                if item.status == AttentionStatus::Open && item.read_at.is_none() {
                    item.read_at = Some(now);
                }
            }
            Ok(())
        })
    }

    pub fn mark_read(&self, id: &str, now: i64) -> Result<()> {
        self.edit_item(id, |item| item.read_at = Some(now))
    }

    pub fn acknowledge(&self, id: &str, now: i64) -> Result<()> {
        self.edit_item(id, |item| {
            item.status = AttentionStatus::Acknowledged;
            item.read_at.get_or_insert(now);
            item.acknowledged_at = Some(now);
            item.snoozed_until = None;
        })
    }

    pub fn snooze(&self, id: &str, until: i64) -> Result<()> {
        self.edit_item(id, |item| {
            if item.status == AttentionStatus::Open {
                item.snoozed_until = Some(until);
                // Snooze is an explicit request for a fresh reminder. Reading
                // the item otherwise suppresses automatic notification.
                item.read_at = None;
                item.notified_at = None;
                item.next_notify_at = None;
                item.notify_failures = 0;
            }
        })
    }

    /// Notify every currently eligible item. The state guard is held while a
    /// bounded transport runs, preventing duplicate sends from another TTYbird
    /// process. Success is persisted only after the transport exits cleanly.
    pub fn notify_pending(&self, now: i64) -> Result<NotificationReport> {
        self.notify_pending_with(now, &SystemNotifier)
    }

    fn edit_item(&self, id: &str, edit: impl FnOnce(&mut AttentionItem)) -> Result<()> {
        self.mutate(|state| {
            let item = state
                .items
                .iter_mut()
                .find(|item| item.id == id)
                .with_context(|| format!("attention item not found: {id}"))?;
            edit(item);
            Ok(())
        })
    }

    fn notify_pending_with(
        &self,
        now: i64,
        notifier: &impl Notifier,
    ) -> Result<NotificationReport> {
        self.mutate(|state| {
            expire_and_bound(state, now);
            let mut report = NotificationReport::default();
            let mut attempted = 0;
            for item in &mut state.items {
                if !item.wants_notification(now) {
                    report.skipped += 1;
                    continue;
                }
                if attempted >= MAX_NOTIFICATIONS_PER_RUN {
                    report.skipped += 1;
                    continue;
                }
                attempted += 1;
                match notifier.send(item, now) {
                    Ok(()) => {
                        item.notified_at = Some(now);
                        item.next_notify_at = None;
                        item.notify_failures = 0;
                        report.sent += 1;
                    }
                    Err(_) => {
                        item.notify_failures = item.notify_failures.saturating_add(1);
                        item.next_notify_at =
                            Some(now.saturating_add(notification_backoff(item.notify_failures)));
                        report.failed += 1;
                    }
                }
            }
            Ok(report)
        })
    }

    fn mutate<T>(&self, f: impl FnOnce(&mut PersistedState) -> Result<T>) -> Result<T> {
        let _guard = StateGuard::acquire(&self.config_dir)?;
        let mut state = load_state(&self.config_dir)?;
        let before = serde_json::to_vec(&state)?;
        let result = f(&mut state)?;
        save_if_changed(&self.config_dir, before, &mut state)?;
        Ok(result)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookMeaning {
    Attention(AttentionKind),
    Transition,
}

fn apply_session(state: &mut PersistedState, session: &Session, now: i64) {
    if session.confidence != Confidence::Observed || session.provider != Provider::Claude {
        return;
    }
    let (Some(pid), Some(process_started_at), Some(observed_at)) =
        (session.pid, session.process_started_at, session.updated_at)
    else {
        return;
    };
    let Some((event, meaning)) = hook_meaning(session) else {
        return;
    };
    let key = SessionKey {
        host: session.host.clone(),
        provider: session.provider,
        session_id: session.id.clone(),
        pid,
        process_started_at,
    };
    let track_index = state.tracks.iter().position(|track| track.key == key);
    let track_index = track_index.unwrap_or_else(|| {
        state.tracks.push(Track {
            key: key.clone(),
            occurrence: 0,
            last_observed_at: i64::MIN,
            last_event: String::new(),
            active_item_id: None,
        });
        state.tracks.len() - 1
    });

    let track = &state.tracks[track_index];
    let is_new = observed_at > track.last_observed_at
        || (observed_at == track.last_observed_at && event != track.last_event);
    if !is_new {
        if let Some(item) = track
            .active_item_id
            .as_deref()
            .and_then(|id| state.items.iter_mut().find(|item| item.id == id))
        {
            let title = sanitized_title(session.insights.title.as_deref());
            if title != item.title {
                item.title = title;
            }
        }
        return;
    }

    if let Some(id) = state.tracks[track_index].active_item_id.take()
        && let Some(item) = state.items.iter_mut().find(|item| item.id == id)
        && item.status == AttentionStatus::Open
    {
        item.status = AttentionStatus::Resolved;
    }
    state.tracks[track_index].last_observed_at = observed_at;
    state.tracks[track_index].last_event = event.to_owned();

    let HookMeaning::Attention(kind) = meaning else {
        return;
    };
    state.tracks[track_index].occurrence = state.tracks[track_index].occurrence.saturating_add(1);
    let occurrence = state.tracks[track_index].occurrence;
    let id = event_id(&key, occurrence);
    state.items.push(AttentionItem {
        id: id.clone(),
        kind,
        status: AttentionStatus::Open,
        host: key.host.clone(),
        provider: key.provider,
        session_id: key.session_id.clone(),
        pid: key.pid,
        process_started_at: key.process_started_at,
        occurrence,
        title: sanitized_title(session.insights.title.as_deref()),
        observed_at,
        opened_at: now,
        last_seen_at: now,
        read_at: None,
        acknowledged_at: None,
        snoozed_until: None,
        notified_at: None,
        next_notify_at: None,
        notify_failures: 0,
    });
    state.tracks[track_index].active_item_id = Some(id);
}

fn hook_meaning(session: &Session) -> Option<(&str, HookMeaning)> {
    let event = hook_event(&session.evidence)?;
    let meaning = match event {
        "PermissionRequest" | "Notification:permission_prompt"
            if session.activity == Activity::WaitingInput =>
        {
            HookMeaning::Attention(AttentionKind::PermissionRequest)
        }
        "Notification:elicitation_dialog"
        | "Notification:elicitation_url_dialog"
        | "Notification:agent_needs_input"
            if session.activity == Activity::WaitingInput =>
        {
            HookMeaning::Attention(AttentionKind::InputRequest)
        }
        // Older TTYbird hook records did not retain the notification subtype.
        // Their WaitingInput mapping was allowlisted, but no longer proves the
        // more specific permission category.
        "Notification" if session.activity == Activity::WaitingInput => {
            HookMeaning::Attention(AttentionKind::InputRequest)
        }
        "Stop" if session.activity == Activity::Idle => {
            HookMeaning::Attention(AttentionKind::ResponseFinished)
        }
        "PostToolUseFailure" if session.activity == Activity::Working => {
            HookMeaning::Attention(AttentionKind::ToolError)
        }
        "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "SessionEnd" => {
            HookMeaning::Transition
        }
        _ => return None,
    };
    Some((event, meaning))
}

fn hook_event(evidence: &str) -> Option<&str> {
    let rest = evidence
        .strip_prefix("Claude hook ")
        .or_else(|| evidence.strip_prefix("Claude hook: "))?;
    let event = rest.split_once(';').map_or(rest, |(event, _)| event).trim();
    (!event.is_empty()).then_some(event)
}

fn expire_and_bound(state: &mut PersistedState, now: i64) {
    for item in &mut state.items {
        let ordinary_expiry = item.observed_at.saturating_add(STALE_AFTER_SECONDS);
        // Snooze is an explicit reminder request. Keep that historical event
        // until shortly after the reminder is due, without claiming the agent
        // is still waiting. A newer observed transition resolves it earlier.
        let expiry = item.snoozed_until.map_or(ordinary_expiry, |until| {
            ordinary_expiry.max(until.saturating_add(STALE_AFTER_SECONDS))
        });
        if item.status == AttentionStatus::Open && now > expiry {
            item.status = AttentionStatus::Expired;
        }
    }
    state.items.retain(|item| {
        item.status == AttentionStatus::Open || now.saturating_sub(item.opened_at) <= RETAIN_SECONDS
    });
    state
        .items
        .sort_by_key(|item| (item.opened_at, item.observed_at));
    if state.items.len() > MAX_ITEMS {
        state.items.drain(..state.items.len() - MAX_ITEMS);
    }
    state.tracks.retain(|track| {
        now.saturating_sub(track.last_observed_at) <= RETAIN_SECONDS
            || track
                .active_item_id
                .as_ref()
                .is_some_and(|id| state.items.iter().any(|item| &item.id == id))
    });
}

fn sorted_items(state: &PersistedState) -> Vec<AttentionItem> {
    let mut items = state.items.clone();
    items.sort_by(|left, right| {
        right
            .opened_at
            .cmp(&left.opened_at)
            .then_with(|| right.observed_at.cmp(&left.observed_at))
            .then_with(|| right.id.cmp(&left.id))
    });
    items
}

fn inbox_update(state: &PersistedState, now: i64) -> InboxUpdate {
    let items = sorted_items(state);
    let notification_candidates = items
        .iter()
        .filter(|item| item.wants_notification(now))
        .map(|item| item.id.clone())
        .collect();
    InboxUpdate {
        items,
        notification_candidates,
    }
}

fn event_id(key: &SessionKey, occurrence: u64) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    let identity = format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        key.host,
        key.provider.as_str(),
        key.session_id,
        key.pid,
        key.process_started_at,
        occurrence
    );
    for byte in identity.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("attention-{hash:016x}-{occurrence}")
}

fn sanitized_title(title: Option<&str>) -> Option<String> {
    let cleaned: String = title?
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        return None;
    }
    const MAX_TITLE_CHARS: usize = 80;
    if cleaned.chars().count() <= MAX_TITLE_CHARS {
        Some(cleaned)
    } else {
        Some(
            cleaned
                .chars()
                .take(MAX_TITLE_CHARS - 1)
                .chain(std::iter::once('…'))
                .collect(),
        )
    }
}

fn notification_backoff(failures: u8) -> i64 {
    let shift = u32::from(failures.saturating_sub(1).min(5));
    30_i64.saturating_mul(1_i64 << shift).min(15 * 60)
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("attention.json")
}

fn load_state(dir: &Path) -> Result<PersistedState> {
    let path = state_path(dir);
    if !path.exists() {
        return Ok(PersistedState {
            version: STATE_VERSION,
            ..PersistedState::default()
        });
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let state: PersistedState = serde_json::from_slice(&bytes).context("invalid attention.json")?;
    if state.version != 0 && state.version != STATE_VERSION {
        bail!("unsupported attention state version: {}", state.version);
    }
    Ok(state)
}

fn save_if_changed(dir: &Path, before: Vec<u8>, state: &mut PersistedState) -> Result<()> {
    state.version = STATE_VERSION;
    if before == serde_json::to_vec(state)? {
        return Ok(());
    }
    config::atomic_write(&state_path(dir), &serde_json::to_vec_pretty(state)?)
}

struct StateGuard(PathBuf);

impl StateGuard {
    fn acquire(dir: &Path) -> Result<Self> {
        Self::try_acquire(dir, Duration::from_millis(750))?.with_context(|| {
            format!(
                "attention state is busy or a writer was interrupted: {}; inspect before removing",
                dir.join("attention.mutation.lock").display()
            )
        })
    }

    fn try_acquire(dir: &Path, wait: Duration) -> Result<Option<Self>> {
        fs::create_dir_all(dir)?;
        let path = dir.join("attention.mutation.lock");
        let deadline = Instant::now() + wait;
        loop {
            let mut options = fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(_) => return Ok(Some(Self(path))),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Ok(None);
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create attention guard {}", path.display()));
                }
            }
        }
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

trait Notifier {
    fn send(&self, item: &AttentionItem, now: i64) -> Result<()>;
}

struct SystemNotifier;

impl Notifier for SystemNotifier {
    fn send(&self, item: &AttentionItem, now: i64) -> Result<()> {
        let title = "TTYbird";
        let body = notification_body(item, now);
        #[cfg(target_os = "macos")]
        {
            let mut command = Command::new("osascript");
            command.args([
                "-e",
                "on run argv\ndisplay notification (item 1 of argv) with title (item 2 of argv)\nend run",
                "--",
                &body,
                title,
            ]);
            run_bounded(&mut command, NOTIFY_TIMEOUT)
        }
        #[cfg(target_os = "linux")]
        {
            let mut command = Command::new("notify-send");
            command.args([
                "--app-name",
                "TTYbird",
                "--expire-time",
                "5000",
                "--",
                title,
                &body,
            ]);
            run_bounded(&mut command, NOTIFY_TIMEOUT)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (title, body);
            bail!("desktop notifications are unsupported on this platform")
        }
    }
}

fn notification_body(item: &AttentionItem, now: i64) -> String {
    let historical = now.saturating_sub(item.observed_at) > STALE_AFTER_SECONDS;
    let description = if historical {
        let noun = match item.kind {
            AttentionKind::PermissionRequest | AttentionKind::InputRequest => "request",
            AttentionKind::ResponseFinished | AttentionKind::ToolError => "event",
        };
        format!("Reminder of observed {noun}: {}", item.kind.label())
    } else {
        item.kind.label().to_owned()
    };
    let mut body = format!(
        "{} on {}: {description}",
        item.provider.label(),
        sanitized_title(Some(&item.host)).unwrap_or_else(|| "host".to_owned())
    );
    if let Some(title) = &item.title {
        body.push_str(" — ");
        body.push_str(title);
    }
    body
}

fn run_bounded(command: &mut Command, timeout: Duration) -> Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start desktop notification transport")?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(anyhow!("desktop notification transport exited {status}"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("desktop notification transport timed out");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionInsights;
    use std::cell::Cell;

    fn session(event: &str, activity: Activity, observed_at: i64) -> Session {
        Session {
            id: "session-1".to_owned(),
            provider: Provider::Claude,
            parent_id: None,
            host: "local".to_owned(),
            pid: Some(42),
            process_started_at: Some(100),
            tty: None,
            cwd: None,
            model: None,
            activity,
            confidence: Confidence::Observed,
            evidence: format!("Claude hook {event}; lifecycle event; PID/start verified"),
            updated_at: Some(observed_at),
            target: None,
            insights: SessionInsights {
                title: Some(" Fix\nlogin\tflow ".to_owned()),
                ..SessionInsights::default()
            },
        }
    }

    fn snapshot(session: Session, now: i64) -> Snapshot {
        Snapshot {
            protocol_version: 1,
            host: "local".to_owned(),
            collected_at: now,
            sessions: vec![session],
            warnings: vec![],
        }
    }

    #[test]
    fn sustained_wait_deduplicates_and_a_new_observation_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        let first = snapshot(
            session(
                "Notification:permission_prompt",
                Activity::WaitingInput,
                100,
            ),
            100,
        );
        let initial = inbox.update(std::slice::from_ref(&first), 100).unwrap();
        assert_eq!(initial.items.len(), 1);
        assert_eq!(initial.items[0].title.as_deref(), Some("Fix login flow"));
        let repeated = inbox.update(&[first], 101).unwrap();
        assert_eq!(repeated.items.len(), 1);
        assert_eq!(repeated.items[0].occurrence, 1);

        // A sustained refresh performs no atomic replacement.
        fs::write(dir.path().join("attention.lock"), b"fixture").unwrap();
        inbox
            .update(
                &[snapshot(
                    session(
                        "Notification:permission_prompt",
                        Activity::WaitingInput,
                        100,
                    ),
                    102,
                )],
                102,
            )
            .unwrap();
        fs::remove_file(dir.path().join("attention.lock")).unwrap();

        let id = repeated.items[0].id.clone();
        inbox.acknowledge(&id, 103).unwrap();
        let next = snapshot(
            session(
                "Notification:permission_prompt",
                Activity::WaitingInput,
                110,
            ),
            110,
        );
        let reopened = inbox.update(&[next], 110).unwrap();
        assert_eq!(reopened.items.len(), 2);
        assert_eq!(reopened.items[0].status, AttentionStatus::Open);
        assert_eq!(reopened.items[0].occurrence, 2);
        assert_ne!(reopened.items[0].id, id);
    }

    #[test]
    fn working_transition_then_wait_creates_a_new_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        inbox
            .update(
                &[snapshot(
                    session(
                        "Notification:agent_needs_input",
                        Activity::WaitingInput,
                        100,
                    ),
                    100,
                )],
                100,
            )
            .unwrap();
        inbox
            .update(
                &[snapshot(
                    session("UserPromptSubmit", Activity::Working, 101),
                    101,
                )],
                101,
            )
            .unwrap();
        let update = inbox
            .update(
                &[snapshot(
                    session(
                        "Notification:agent_needs_input",
                        Activity::WaitingInput,
                        102,
                    ),
                    102,
                )],
                102,
            )
            .unwrap();
        assert_eq!(update.items[0].occurrence, 2);
        assert_eq!(update.items[0].status, AttentionStatus::Open);
        assert_eq!(update.items[1].status, AttentionStatus::Resolved);
    }

    #[test]
    fn pre_tool_use_is_never_attention_and_hook_kinds_are_precise() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        let update = inbox
            .update(
                &[snapshot(
                    session("PreToolUse", Activity::WaitingTool, 100),
                    100,
                )],
                100,
            )
            .unwrap();
        assert!(update.items.is_empty());

        let finished = inbox
            .update(&[snapshot(session("Stop", Activity::Idle, 101), 101)], 101)
            .unwrap();
        assert_eq!(finished.items[0].kind, AttentionKind::ResponseFinished);
        let failed = inbox
            .update(
                &[snapshot(
                    session("PostToolUseFailure", Activity::Working, 102),
                    102,
                )],
                102,
            )
            .unwrap();
        assert_eq!(failed.items[0].kind, AttentionKind::ToolError);
    }

    #[test]
    fn pre_tool_transition_resolves_an_older_snoozed_request() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        let opened = inbox
            .update(
                &[snapshot(
                    session(
                        "Notification:permission_prompt",
                        Activity::WaitingInput,
                        100,
                    ),
                    100,
                )],
                100,
            )
            .unwrap();
        inbox.snooze(&opened.items[0].id, 701).unwrap();
        let proceeded = inbox
            .update(
                &[snapshot(
                    session("PreToolUse", Activity::WaitingTool, 101),
                    101,
                )],
                101,
            )
            .unwrap();
        assert_eq!(proceeded.items[0].status, AttentionStatus::Resolved);
        assert!(proceeded.notification_candidates.is_empty());
    }

    #[test]
    fn generic_legacy_notification_is_input_and_unobserved_state_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        let legacy = inbox
            .update(
                &[snapshot(
                    session("Notification", Activity::WaitingInput, 100),
                    100,
                )],
                100,
            )
            .unwrap();
        assert_eq!(legacy.items[0].kind, AttentionKind::InputRequest);

        let mut inferred = session(
            "Notification:permission_prompt",
            Activity::WaitingInput,
            101,
        );
        inferred.confidence = Confidence::Inferred;
        let unchanged = inbox.update(&[snapshot(inferred, 101)], 101).unwrap();
        assert_eq!(unchanged.items.len(), 1);
        inbox.mark_all_read(102).unwrap();
        assert_eq!(inbox.items(102).unwrap()[0].read_at, Some(102));
    }

    #[test]
    fn ten_minute_snooze_becomes_a_historical_reminder_then_expires() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        let update = inbox
            .update(
                &[snapshot(
                    session(
                        "Notification:elicitation_dialog",
                        Activity::WaitingInput,
                        100,
                    ),
                    100,
                )],
                100,
            )
            .unwrap();
        let id = update.items[0].id.clone();
        inbox.mark_read(&id, 101).unwrap();
        assert!(
            inbox
                .update(&[], 101)
                .unwrap()
                .notification_candidates
                .is_empty()
        );
        inbox.snooze(&id, 701).unwrap();
        assert!(
            inbox
                .update(&[], 401)
                .unwrap()
                .notification_candidates
                .is_empty()
        );
        let reminder = inbox.update(&[], 701).unwrap();
        assert_eq!(reminder.notification_candidates, vec![id]);
        assert!(
            notification_body(&reminder.items[0], 701)
                .contains("Reminder of observed request: Input requested")
        );
        let stale = inbox.update(&[], 1002).unwrap();
        assert_eq!(stale.items[0].status, AttentionStatus::Expired);
    }

    #[test]
    fn unreachable_unsnoozed_item_expires_without_becoming_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        inbox
            .update(
                &[snapshot(
                    session(
                        "Notification:permission_prompt",
                        Activity::WaitingInput,
                        100,
                    ),
                    100,
                )],
                100,
            )
            .unwrap();
        let stale = inbox.update(&[], 401).unwrap();
        assert_eq!(stale.items[0].status, AttentionStatus::Expired);
    }

    struct FakeNotifier {
        calls: Cell<usize>,
        fail: bool,
    }

    impl Notifier for FakeNotifier {
        fn send(&self, _item: &AttentionItem, _now: i64) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                bail!("fixture failure")
            }
            Ok(())
        }
    }

    #[test]
    fn successful_notification_is_deduplicated_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        inbox
            .update(
                &[snapshot(
                    session(
                        "Notification:permission_prompt",
                        Activity::WaitingInput,
                        100,
                    ),
                    100,
                )],
                100,
            )
            .unwrap();
        let notifier = FakeNotifier {
            calls: Cell::new(0),
            fail: false,
        };
        assert_eq!(inbox.notify_pending_with(100, &notifier).unwrap().sent, 1);
        let restarted = AttentionInbox::new(dir.path());
        assert_eq!(
            restarted.notify_pending_with(101, &notifier).unwrap().sent,
            0
        );
        assert_eq!(notifier.calls.get(), 1);
    }

    #[test]
    fn failed_notification_is_not_marked_sent_and_has_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = AttentionInbox::new(dir.path());
        inbox
            .update(
                &[snapshot(
                    session(
                        "Notification:permission_prompt",
                        Activity::WaitingInput,
                        100,
                    ),
                    100,
                )],
                100,
            )
            .unwrap();
        let notifier = FakeNotifier {
            calls: Cell::new(0),
            fail: true,
        };
        assert_eq!(inbox.notify_pending_with(100, &notifier).unwrap().failed, 1);
        assert_eq!(inbox.notify_pending_with(110, &notifier).unwrap().sent, 0);
        assert_eq!(notifier.calls.get(), 1);
        assert!(inbox.items(110).unwrap()[0].notified_at.is_none());
    }
}
