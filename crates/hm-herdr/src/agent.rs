//! herdr agent models and JSON deserialization.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{HerdrError, Result};

/// Agent lifecycle status reported by herdr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HerdrStatus {
    /// Agent is idle and waiting for input.
    Idle,
    /// Agent is running a task / prompt.
    Working,
    /// Agent is blocked on user input or a tool.
    Blocked,
    /// Agent has completed its task / session.
    Done,
    /// Status is unknown or unrecognized.
    Unknown,
}

/// Alias for [`HerdrStatus`].
pub type HerdrAgentStatus = HerdrStatus;

impl fmt::Display for HerdrStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Working => write!(f, "working"),
            Self::Blocked => write!(f, "blocked"),
            Self::Done => write!(f, "done"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

impl From<&str> for HerdrStatus {
    fn from(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }
}

impl From<String> for HerdrStatus {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl TryFrom<HerdrStatus> for hm_proto::AgentStatus {
    type Error = HerdrError;

    fn try_from(status: HerdrStatus) -> Result<Self, Self::Error> {
        match status {
            HerdrStatus::Idle => Ok(Self::Idle),
            HerdrStatus::Working => Ok(Self::Working),
            HerdrStatus::Blocked => Ok(Self::Blocked),
            HerdrStatus::Done => Ok(Self::Done),
            HerdrStatus::Unknown => Err(HerdrError::UnknownStatus),
        }
    }
}

impl From<hm_proto::AgentStatus> for HerdrStatus {
    fn from(status: hm_proto::AgentStatus) -> Self {
        match status {
            hm_proto::AgentStatus::Idle => Self::Idle,
            hm_proto::AgentStatus::Working => Self::Working,
            hm_proto::AgentStatus::Blocked => Self::Blocked,
            hm_proto::AgentStatus::Done => Self::Done,
        }
    }
}

/// Agent pane representation parsed from herdr CLI output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HerdrAgent {
    /// Agent name, if assigned.
    pub name: Option<String>,
    /// Underlying agent kind (e.g. "claude", "codex", "agy").
    pub kind: String,
    /// Lifecycle status reported by herdr.
    pub status: HerdrStatus,
    /// Terminal pane identifier (e.g. "w2:p1").
    pub pane_id: String,
    /// Working directory of the agent pane.
    pub cwd: PathBuf,
}

impl HerdrAgent {
    /// Create a new agent representation.
    pub fn new(
        name: impl Into<Option<String>>,
        kind: impl Into<String>,
        status: HerdrStatus,
        pane_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            status,
            pane_id: pane_id.into(),
            cwd: cwd.into(),
        }
    }

    /// Best target identifier for `herdr agent prompt` (name if available, else pane_id).
    pub fn target(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.pane_id)
    }
}

#[derive(Debug, Deserialize)]
struct RawAgentListResponse {
    result: RawAgentListResult,
}

#[derive(Debug, Deserialize)]
struct RawAgentListResult {
    agents: Vec<RawAgent>,
}

#[derive(Debug, Deserialize)]
struct RawAgent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    #[allow(dead_code)]
    workspace_id: Option<String>,
}

impl From<RawAgent> for HerdrAgent {
    fn from(raw: RawAgent) -> Self {
        Self {
            name: raw.name,
            kind: raw.agent,
            status: HerdrStatus::from(raw.agent_status.as_str()),
            pane_id: raw.pane_id,
            cwd: raw.cwd.unwrap_or_default(),
        }
    }
}

/// Parse the JSON output of `herdr agent list` into a list of [`HerdrAgent`] records.
pub fn parse_agent_list_json(json: &str) -> Result<Vec<HerdrAgent>> {
    // 1. Standard herdr CLI format: {"result":{"agents":[...]}}
    if let Ok(resp) = serde_json::from_str::<RawAgentListResponse>(json) {
        return Ok(resp
            .result
            .agents
            .into_iter()
            .map(HerdrAgent::from)
            .collect());
    }

    // 2. Direct agents object: {"agents":[...]}
    if let Ok(wrap) = serde_json::from_str::<RawAgentListResult>(json) {
        return Ok(wrap.agents.into_iter().map(HerdrAgent::from).collect());
    }

    // 3. Raw array fallback: [...]
    let raw_list: Vec<RawAgent> = serde_json::from_str(json)?;
    Ok(raw_list.into_iter().map(HerdrAgent::from).collect())
}

/// Strip all ASCII/Unicode control characters, newlines and carriage returns from `s`.
pub fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            !c.is_control() && c != '\r' && c != '\n' && c != '\u{2028}' && c != '\u{2029}'
        })
        .collect()
}
