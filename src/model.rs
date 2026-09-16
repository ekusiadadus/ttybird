use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Codex,
    Claude,
    Gemini,
    OpenCode,
    Amp,
    Cursor,
    Copilot,
    Aider,
    Goose,
    Cline,
    QwenCode,
    Kilo,
    Droid,
    Crush,
    Pi,
    MistralVibe,
    #[serde(other)]
    Unknown,
}

impl Provider {
    pub const ALL: &'static [Self] = &[
        Self::Codex,
        Self::Claude,
        Self::Gemini,
        Self::OpenCode,
        Self::Amp,
        Self::Cursor,
        Self::Copilot,
        Self::Aider,
        Self::Goose,
        Self::Cline,
        Self::QwenCode,
        Self::Kilo,
        Self::Droid,
        Self::Crush,
        Self::Pi,
        Self::MistralVibe,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::OpenCode => "open_code",
            Self::Amp => "amp",
            Self::Cursor => "cursor",
            Self::Copilot => "copilot",
            Self::Aider => "aider",
            Self::Goose => "goose",
            Self::Cline => "cline",
            Self::QwenCode => "qwen_code",
            Self::Kilo => "kilo",
            Self::Droid => "droid",
            Self::Crush => "crush",
            Self::Pi => "pi",
            Self::MistralVibe => "mistral_vibe",
            Self::Unknown => "unknown",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Gemini => "Gemini",
            Self::OpenCode => "OpenCode",
            Self::Amp => "Amp",
            Self::Cursor => "Cursor",
            Self::Copilot => "Copilot",
            Self::Aider => "Aider",
            Self::Goose => "Goose",
            Self::Cline => "Cline",
            Self::QwenCode => "Qwen Code",
            Self::Kilo => "Kilo",
            Self::Droid => "Droid",
            Self::Crush => "Crush",
            Self::Pi => "Pi",
            Self::MistralVibe => "Vibe",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Working,
    WaitingInput,
    WaitingTool,
    Idle,
    Ended,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Observed,
    Inferred,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenUsageScope {
    /// A provider-supplied cumulative total, or a complete transcript sum.
    Total,
    /// A sum from the bounded portions of a transcript that were inspected.
    Sampled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    /// All input tokens. A reported cached input count is a subset.
    pub input_tokens: u64,
    /// `None` when the provider did not report this field.
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: u64,
    /// Reported reasoning/thinking output, already included in output.
    /// `None` when the provider did not report this field.
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: u64,
    pub scope: TokenUsageScope,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInsights {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_observation: Option<ActivityObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::workspace::WorkspaceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<String>,
    pub title: Option<String>,
    pub usage: Option<TokenUsage>,
    /// Collection-only identity for explicit, revalidated local conversation reads.
    #[serde(skip)]
    pub(crate) log_path: Option<PathBuf>,
}

/// State evidence has its own clock: a log's modification time is not activity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityObservation {
    pub source: String,
    pub event: String,
    pub observed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Managed {
        session_id: String,
    },
    Ghostty {
        terminal_id: String,
    },
    Tmux {
        socket: Option<String>,
        pane: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub provider: Provider,
    pub parent_id: Option<String>,
    pub host: String,
    pub pid: Option<u32>,
    pub process_started_at: Option<u64>,
    pub tty: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub activity: Activity,
    pub confidence: Confidence,
    pub evidence: String,
    pub updated_at: Option<i64>,
    pub target: Option<Target>,
    #[serde(default)]
    pub insights: SessionInsights,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol_version: u32,
    pub host: String,
    pub collected_at: i64,
    pub sessions: Vec<Session>,
    pub warnings: Vec<String>,
}

impl Snapshot {
    pub fn new(host: String) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            host,
            collected_at: chrono::Utc::now().timestamp(),
            sessions: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod provider_tests {
    use super::{Provider, Session};

    #[test]
    fn provider_names_match_serialized_values_and_unknown_is_forward_compatible() {
        for provider in Provider::ALL {
            assert_eq!(
                serde_json::to_string(provider).unwrap(),
                format!("\"{}\"", provider.as_str())
            );
        }
        assert_eq!(
            serde_json::from_str::<Provider>("\"future_agent\"").unwrap(),
            Provider::Unknown
        );
        assert!(!Provider::ALL.contains(&Provider::Unknown));
    }

    #[test]
    fn session_insights_default_for_older_snapshots_and_skip_local_path() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "id":"old",
            "provider":"codex",
            "parent_id":null,
            "host":"local",
            "pid":null,
            "process_started_at":null,
            "tty":null,
            "cwd":null,
            "model":null,
            "activity":"unknown",
            "confidence":"unknown",
            "evidence":"fixture",
            "updated_at":null,
            "target":null
        }))
        .unwrap();
        assert_eq!(session.insights, Default::default());

        let serialized = serde_json::to_value(&session).unwrap();
        assert_eq!(serialized["insights"]["title"], serde_json::Value::Null);
        assert!(serialized["insights"].get("log_path").is_none());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub session_id: String,
    pub host: String,
    pub pid: u32,
    pub process_started_at: u64,
    pub target: Target,
}
