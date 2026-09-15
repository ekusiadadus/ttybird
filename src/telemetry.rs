//! Optional Claude hooks supply lifecycle evidence without retaining conversation content.
use crate::{
    config,
    model::{Activity, Confidence, Provider, Session, Snapshot},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    pub session_id: String,
    pub pid: u32,
    pub process_started_at: u64,
    pub cwd: Option<String>,
    pub activity: Activity,
    pub event: String,
    pub timestamp: i64,
}

fn process_snapshot() -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .without_tasks(),
    );
    system
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 160
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

pub fn activity(value: &serde_json::Value) -> Option<Activity> {
    match value.get("hook_event_name")?.as_str()? {
        "SessionStart" | "Stop" => Some(Activity::Idle),
        "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" => Some(Activity::Working),
        "PreToolUse" => Some(Activity::WaitingTool),
        "SessionEnd" => Some(Activity::Ended),
        "Notification" if value["notification_type"] == "permission_prompt" => {
            Some(Activity::WaitingInput)
        }
        "Notification" if value["notification_type"] == "idle_prompt" => Some(Activity::Idle),
        _ => None,
    }
}

fn claude_ancestor(system: &System) -> Option<(u32, u64)> {
    let mut pid = Pid::from_u32(std::process::id());
    for _ in 0..32 {
        let process = system.process(pid)?;
        if process.name().to_string_lossy() == "claude"
            || process
                .exe()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "claude")
        {
            return Some((pid.as_u32(), process.start_time()));
        }
        let parent = process.parent()?;
        if parent == pid {
            break;
        }
        pid = parent;
    }
    None
}

pub fn record(dir: &Path, input: impl Read) -> Result<()> {
    let mut bytes = Vec::new();
    input.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        bail!("hook payload exceeds 64 KiB; no content saved");
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).context("invalid hook JSON")?;
    let Some(activity) = activity(&value) else {
        return Ok(());
    };
    let id = value["session_id"]
        .as_str()
        .filter(|id| valid_id(id))
        .context("invalid session_id")?;
    let system = process_snapshot();
    let (pid, process_started_at) =
        claude_ancestor(&system).context("no Claude ancestor; event not associated")?;
    let event = Event {
        session_id: id.to_string(),
        pid,
        process_started_at,
        cwd: value["cwd"].as_str().map(str::to_string),
        activity,
        event: value["hook_event_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        timestamp: chrono::Utc::now().timestamp(),
    };
    config::atomic_write(
        &dir.join("events").join(format!("{id}.json")),
        &serde_json::to_vec(&event)?,
    )
}

pub fn enrich(dir: &Path, snapshot: &mut Snapshot) -> Result<()> {
    let path = dir.join("events");
    if !path.exists() {
        return Ok(());
    }
    let system = process_snapshot();
    for entry in std::fs::read_dir(path)?.take(1024) {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.metadata()?.len() > 8192 {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Event>(&std::fs::read(entry.path())?) else {
            continue;
        };
        let live = system.process(Pid::from_u32(event.pid)).is_some_and(|p| {
            crate::collect::process_is_live(p.status())
                && p.start_time() == event.process_started_at
        });
        if !live {
            continue;
        }
        let age = snapshot.collected_at.saturating_sub(event.timestamp);
        let fresh = (0..=300).contains(&age);
        let activity = if fresh {
            event.activity
        } else {
            Activity::Unknown
        };
        let evidence = format!(
            "Claude hook {}; {}",
            event.event,
            if fresh {
                "lifecycle event; PID/start verified"
            } else {
                "stale (>5m); PID/start verified"
            }
        );
        let session = Session {
            id: event.session_id.clone(),
            provider: Provider::Claude,
            parent_id: None,
            host: snapshot.host.clone(),
            pid: Some(event.pid),
            process_started_at: Some(event.process_started_at),
            tty: None,
            cwd: event.cwd,
            model: None,
            insights: Default::default(),
            activity,
            confidence: if fresh {
                Confidence::Observed
            } else {
                Confidence::Unknown
            },
            evidence,
            updated_at: Some(event.timestamp),
            target: None,
        };
        if let Some(existing) = snapshot.sessions.iter_mut().find(|s| {
            s.id == event.session_id
                || (s.pid == Some(event.pid)
                    && s.provider == Provider::Claude
                    && s.id.starts_with("claude-pid-"))
        }) {
            merge_observation(existing, session);
        } else {
            snapshot.sessions.push(session);
        }
    }
    Ok(())
}

fn merge_observation(existing: &mut Session, mut observed: Session) {
    if existing.pid == observed.pid && existing.process_started_at == observed.process_started_at {
        observed.tty = existing.tty.clone();
        observed.target = existing.target.clone();
    }
    observed.model = existing.model.clone();
    *existing = observed;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resumed_hook_never_inherits_another_process_terminal() {
        let old:Session=serde_json::from_value(serde_json::json!({"id":"session","provider":"claude","parent_id":null,"host":"local","pid":10,"process_started_at":100,"tty":"/dev/pts/1","cwd":null,"model":"fixture","activity":"unknown","confidence":"observed","evidence":"fixture","updated_at":null,"target":{"kind":"tmux","socket":null,"pane":"%1"}})).unwrap();
        let mut new = old.clone();
        new.pid = Some(20);
        new.process_started_at = Some(200);
        new.tty = None;
        new.target = None;
        let mut merged = old.clone();
        merge_observation(&mut merged, new);
        assert_eq!(merged.pid, Some(20));
        assert!(merged.tty.is_none());
        assert!(merged.target.is_none());
        let mut same = old.clone();
        same.tty = None;
        same.target = None;
        let mut merged = old;
        merge_observation(&mut merged, same);
        assert_eq!(merged.tty.as_deref(), Some("/dev/pts/1"));
        assert!(merged.target.is_some());
    }
    #[test]
    fn only_explicit_permission_notification_waits_for_input() {
        assert_eq!(
            activity(
                &serde_json::json!({"hook_event_name":"Notification","notification_type":"permission_prompt"})
            ),
            Some(Activity::WaitingInput)
        );
        assert_eq!(
            activity(
                &serde_json::json!({"hook_event_name":"Notification","notification_type":"idle_prompt"})
            ),
            Some(Activity::Idle)
        );
        assert_eq!(
            activity(&serde_json::json!({"hook_event_name":"SubagentStop"})),
            None
        );
    }
    #[test]
    fn rejects_path_traversal() {
        assert!(!valid_id("../../evil"));
        assert!(!valid_id("x\n"));
        assert!(valid_id("abc-123_def"));
    }
    #[test]
    fn oversized_hook_is_not_written() {
        let d = tempfile::tempdir().unwrap();
        assert!(record(d.path(), vec![b' '; 70_000].as_slice()).is_err());
        assert!(!d.path().join("events").exists());
    }
}
