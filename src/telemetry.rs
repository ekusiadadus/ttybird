//! Optional Claude hooks supply lifecycle evidence without retaining conversation content.
use crate::{
    config,
    model::{Activity, ActivityObservation, Confidence, Provider, Session, Snapshot},
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
        "PermissionRequest" => Some(Activity::WaitingInput),
        "SessionEnd" => Some(Activity::Ended),
        "Notification" if value["notification_type"] == "permission_prompt" => {
            Some(Activity::WaitingInput)
        }
        "Notification" if value["notification_type"] == "idle_prompt" => Some(Activity::Idle),
        "Notification"
            if matches!(
                value["notification_type"].as_str(),
                Some("elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input")
            ) =>
        {
            Some(Activity::WaitingInput)
        }
        _ => None,
    }
}

fn event_evidence(value: &serde_json::Value) -> String {
    if value["hook_event_name"] == "Notification" {
        format!(
            "Notification:{}",
            value["notification_type"].as_str().unwrap_or("unknown")
        )
    } else {
        value["hook_event_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string()
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
        event: event_evidence(&value),
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
        let insights = crate::model::SessionInsights {
            activity_observation: Some(ActivityObservation {
                source: "claude_hook".to_owned(),
                event: event.event.clone(),
                observed_at: event.timestamp,
            }),
            ..Default::default()
        };
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
            insights,
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
    let same_identity =
        existing.pid == observed.pid && existing.process_started_at == observed.process_started_at;
    let existing_is_newer_known = same_identity
        && existing.activity != Activity::Unknown
        && existing.confidence != Confidence::Unknown
        && existing
            .insights
            .activity_observation
            .as_ref()
            .zip(observed.insights.activity_observation.as_ref())
            .is_some_and(|(existing, observed)| existing.observed_at > observed.observed_at);
    if existing_is_newer_known {
        return;
    }
    if same_identity {
        let activity_observation = observed.insights.activity_observation.take();
        observed.tty = existing.tty.clone();
        observed.target = existing.target.clone();
        observed.insights = existing.insights.clone();
        observed.insights.activity_observation = activity_observation;
        observed.parent_id = existing.parent_id.clone();
    }
    observed.model = existing.model.clone();
    *existing = observed;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(source: &str, event: &str, observed_at: i64) -> ActivityObservation {
        ActivityObservation {
            source: source.to_owned(),
            event: event.to_owned(),
            observed_at,
        }
    }

    #[test]
    fn older_stale_hook_does_not_replace_newer_known_log_activity() {
        let mut existing: Session = serde_json::from_value(serde_json::json!({
            "id":"session","provider":"claude","parent_id":null,"host":"local",
            "pid":10,"process_started_at":100,"tty":"/dev/pts/1","cwd":"/work",
            "model":"fixture","activity":"idle","confidence":"inferred",
            "evidence":"newer transcript lifecycle","updated_at":200,"target":null
        }))
        .unwrap();
        existing.insights.activity_observation =
            Some(observation("claude_log", "assistant:end_turn", 200));
        let mut hook = existing.clone();
        hook.activity = Activity::Unknown;
        hook.confidence = Confidence::Unknown;
        hook.evidence = "Claude hook PreToolUse; stale (>5m); PID/start verified".to_owned();
        hook.updated_at = Some(100);
        hook.insights.activity_observation = Some(observation("claude_hook", "PreToolUse", 100));

        merge_observation(&mut existing, hook);

        assert_eq!(existing.activity, Activity::Idle);
        assert_eq!(existing.confidence, Confidence::Inferred);
        assert_eq!(existing.updated_at, Some(200));
        assert_eq!(
            existing.insights.activity_observation,
            Some(observation("claude_log", "assistant:end_turn", 200))
        );
    }

    #[test]
    fn newer_hook_replaces_log_activity_and_keeps_other_insights() {
        let mut existing: Session = serde_json::from_value(serde_json::json!({
            "id":"session","provider":"claude","parent_id":"parent","host":"local",
            "pid":10,"process_started_at":100,"tty":"/dev/pts/1","cwd":"/work",
            "model":"fixture","activity":"idle","confidence":"inferred",
            "evidence":"older transcript lifecycle","updated_at":100,
            "target":{"kind":"tmux","socket":null,"pane":"%1"}
        }))
        .unwrap();
        existing.insights.title = Some("Preserved task title".to_owned());
        existing.insights.activity_observation =
            Some(observation("claude_log", "assistant:end_turn", 100));
        let mut hook = existing.clone();
        hook.parent_id = None;
        hook.tty = None;
        hook.target = None;
        hook.activity = Activity::WaitingInput;
        hook.confidence = Confidence::Observed;
        hook.evidence = "Claude hook PermissionRequest; lifecycle event".to_owned();
        hook.updated_at = Some(200);
        hook.insights = Default::default();
        hook.insights.activity_observation =
            Some(observation("claude_hook", "PermissionRequest", 200));

        merge_observation(&mut existing, hook);

        assert_eq!(existing.activity, Activity::WaitingInput);
        assert_eq!(existing.confidence, Confidence::Observed);
        assert_eq!(existing.updated_at, Some(200));
        assert_eq!(existing.tty.as_deref(), Some("/dev/pts/1"));
        assert!(existing.target.is_some());
        assert_eq!(existing.parent_id.as_deref(), Some("parent"));
        assert_eq!(
            existing.insights.title.as_deref(),
            Some("Preserved task title")
        );
        assert_eq!(
            existing.insights.activity_observation,
            Some(observation("claude_hook", "PermissionRequest", 200))
        );
    }

    #[test]
    fn different_process_identity_is_not_suppressed_by_timestamp_ordering() {
        let mut existing: Session = serde_json::from_value(serde_json::json!({
            "id":"session","provider":"claude","parent_id":null,"host":"local",
            "pid":10,"process_started_at":100,"tty":"/dev/pts/1","cwd":"/work",
            "model":"fixture","activity":"idle","confidence":"inferred",
            "evidence":"newer old-process transcript","updated_at":200,
            "target":{"kind":"tmux","socket":null,"pane":"%1"}
        }))
        .unwrap();
        existing.insights.title = Some("Old process title".to_owned());
        existing.insights.activity_observation =
            Some(observation("claude_log", "assistant:end_turn", 200));
        let mut hook = existing.clone();
        hook.pid = Some(20);
        hook.process_started_at = Some(300);
        hook.tty = None;
        hook.target = None;
        hook.activity = Activity::Working;
        hook.confidence = Confidence::Observed;
        hook.updated_at = Some(100);
        hook.insights = Default::default();
        hook.insights.activity_observation =
            Some(observation("claude_hook", "UserPromptSubmit", 100));

        merge_observation(&mut existing, hook);

        assert_eq!(existing.pid, Some(20));
        assert_eq!(existing.process_started_at, Some(300));
        assert_eq!(existing.activity, Activity::Working);
        assert!(existing.tty.is_none());
        assert!(existing.target.is_none());
        assert!(existing.insights.title.is_none());
        assert_eq!(
            existing.insights.activity_observation,
            Some(observation("claude_hook", "UserPromptSubmit", 100))
        );
    }

    #[test]
    fn resumed_hook_never_inherits_another_process_terminal() {
        let old:Session=serde_json::from_value(serde_json::json!({"id":"session","provider":"claude","parent_id":null,"host":"local","pid":10,"process_started_at":100,"tty":"/dev/pts/1","cwd":null,"model":"fixture","activity":"unknown","confidence":"observed","evidence":"fixture","updated_at":null,"target":{"kind":"tmux","socket":null,"pane":"%1"}})).unwrap();
        let mut old = old;
        old.parent_id = Some("parent-session".to_owned());
        old.insights.title = Some("Preserved task title".to_owned());
        let mut new = old.clone();
        new.pid = Some(20);
        new.process_started_at = Some(200);
        new.tty = None;
        new.target = None;
        new.parent_id = None;
        new.insights = Default::default();
        let mut merged = old.clone();
        merge_observation(&mut merged, new);
        assert_eq!(merged.pid, Some(20));
        assert!(merged.tty.is_none());
        assert!(merged.target.is_none());
        assert!(merged.parent_id.is_none());
        assert!(merged.insights.title.is_none());
        let mut same = old.clone();
        same.tty = None;
        same.target = None;
        same.parent_id = None;
        same.insights = Default::default();
        let mut merged = old;
        merge_observation(&mut merged, same);
        assert_eq!(merged.tty.as_deref(), Some("/dev/pts/1"));
        assert!(merged.target.is_some());
        assert_eq!(merged.parent_id.as_deref(), Some("parent-session"));
        assert_eq!(
            merged.insights.title.as_deref(),
            Some("Preserved task title")
        );
    }
    #[test]
    fn permission_requests_are_distinct_from_tools_running() {
        assert_eq!(
            activity(&serde_json::json!({"hook_event_name":"PermissionRequest"})),
            Some(Activity::WaitingInput)
        );
        assert_eq!(
            activity(&serde_json::json!({"hook_event_name":"PreToolUse"})),
            Some(Activity::WaitingTool)
        );
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
    fn notification_subtype_is_retained_as_attention_evidence() {
        for notification_type in [
            "permission_prompt",
            "elicitation_dialog",
            "elicitation_url_dialog",
            "agent_needs_input",
        ] {
            let payload = serde_json::json!({
                "hook_event_name": "Notification",
                "notification_type": notification_type,
            });
            assert_eq!(
                event_evidence(&payload),
                format!("Notification:{notification_type}")
            );
            assert_eq!(activity(&payload), Some(Activity::WaitingInput));
        }
        assert_eq!(
            event_evidence(&serde_json::json!({"hook_event_name":"Stop"})),
            "Stop"
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
